use std::io;
use std::iter::repeat;
use std::borrow::Cow;
use std::cell::RefCell;
use std::time::{Duration, Instant};
use std::sync::mpsc::{channel, Sender, Receiver};

use parking_lot::RwLock;

use term::Term;
use utils::expand_template;
use ansistyle::{style, Styled, measure_text_width};

/// Controls the rendering style of progress bars.
#[derive(Clone)]
pub struct ProgressStyle {
    pub tick_chars: Vec<char>,
    pub progress_styles: Vec<Styled<char>>,
    pub bar_template: Cow<'static, str>,
    pub spinner_template: Cow<'static, str>,
}

/// The drawn state of an element.
#[derive(Clone)]
pub struct DrawState {
    /// The lines to print (can contain ANSI codes)
    pub lines: Vec<String>,
    /// True if the bar no longer needs drawing.
    pub finished: bool,
    /// True if drawing should be forced.
    pub force_draw: bool,
    /// Time when the draw state was created.
    pub ts: Instant,
}

enum Status {
    InProgress,
    DoneVisible,
    DoneHidden,
}

/// Target for draw operations
pub enum DrawTarget {
    /// Draws into a terminal
    Term(Term, Option<DrawState>, Option<Duration>),
    /// Draws to a remote receiver
    Remote(usize, Sender<(usize, DrawState)>),
    /// Do not draw at all
    Hidden,
}

impl DrawTarget {
    /// Draw to a terminal, optionally with a refresh rate.
    pub fn to_term(term: Term, refresh_rate: Option<u64>) -> DrawTarget {
        let rate = refresh_rate.map(|x| Duration::from_millis(1000 / x));
        DrawTarget::Term(term, None, rate)
    }

    /// Draw to a buffered stdout terminal at a max of 15 times a second.
    pub fn stdout() -> DrawTarget {
        DrawTarget::to_term(Term::buffered_stdout(), Some(15))
    }

    /// Draw to a buffered stderr terminal at a max of 15 times a second.
    pub fn stderr() -> DrawTarget {
        DrawTarget::to_term(Term::buffered_stderr(), Some(15))
    }

    /// Apply the given draw state (draws it).
    pub fn apply_draw_state(&mut self, draw_state: DrawState) -> io::Result<()> {
        match *self {
            DrawTarget::Term(ref term, ref mut last_state, rate) => {
                let last_draw = last_state.as_ref().map(|x| x.ts);
                if draw_state.finished ||
                   draw_state.force_draw ||
                   rate.is_none() ||
                   last_draw.is_none() ||
                   last_draw.unwrap().elapsed() > rate.unwrap() {
                    if let Some(ref last_state) = *last_state {
                        //term.move_cursor_up(last_state.lines.len())?;
                        last_state.clear_term(term)?;
                    }
                    draw_state.draw_to_term(term)?;
                    term.flush()?;
                    *last_state = Some(draw_state);
                }
            }
            DrawTarget::Remote(idx, ref chan) => {
                chan.send((idx, draw_state)).unwrap();
            }
            DrawTarget::Hidden => {}
        }
        Ok(())
    }
}

impl DrawState {
    pub fn clear_term(&self, term: &Term) -> io::Result<()> {
        term.clear_last_lines(self.lines.len())
    }

    pub fn draw_to_term(&self, term: &Term) -> io::Result<()> {
        for line in &self.lines {
            term.write_line(line)?;
        }
        Ok(())
    }
}

impl Default for ProgressStyle {
    fn default() -> ProgressStyle {
        ProgressStyle {
            tick_chars: "⠁⠁⠉⠙⠚⠒⠂⠂⠒⠲⠴⠤⠄⠄⠤⠠⠠⠤⠦⠖⠒⠐⠐⠒⠓⠋⠉⠈⠈ ".chars().collect(),
            progress_styles: vec![
                style('█'),
                style('█'),
                style('░')
            ],
            bar_template: Cow::Borrowed("{msg}\n{wide_bar} {pos}/{len}"),
            spinner_template: Cow::Borrowed("{spinner} {msg}"),
        }
    }
}

impl ProgressStyle {

    /// Returns the tick char for a given number.
    pub fn get_tick_char(&self, idx: u64) -> char {
        self.tick_chars[(idx as usize) % (self.tick_chars.len() - 1)]
    }

    /// Returns the tick char for the finished state.
    pub fn get_final_tick_char(&self) -> char {
        self.tick_chars[self.tick_chars.len() - 1]
    }

    pub fn format_bar(&self, state: &ProgressState, width: usize) -> String {
        let pct = state.percent();
        let mut fill = (pct * width as f32) as usize;
        let mut head = 0;
        if fill > 0 && !state.is_finished() {
            fill -= 1;
            head = 1;
        }

        let bar = repeat(state.style.progress_styles[0].to_string())
            .take(fill).collect::<String>();
        let cur = if head == 1 {
            state.style.progress_styles[1].to_string()
        } else {
            "".into()
        };
        let rest = repeat(state.style.progress_styles[2].to_string())
            .take(width - fill - head).collect::<String>();
        format!("{}{}{}", bar, cur, rest)
    }

    pub fn format_state(&self, state: &ProgressState) -> Vec<String> {
        let (pos, len) = state.position();
        let mut rv = vec![];

        for line in self.bar_template.lines() {
            let need_wide_bar = RefCell::new(false);

            let s = expand_template(line, |key| {
                if key == "wide_bar" {
                    *need_wide_bar.borrow_mut() = true;
                    "\x00".into()
                } else if key == "bar" {
                    // XXX: width?
                    self.format_bar(state, 20)
                } else if key == "spinner" {
                    state.current_tick_char().to_string()
                } else if key == "msg" {
                    state.message().to_string()
                } else if key == "pos" {
                    pos.to_string()
                } else if key == "len" {
                    len.to_string()
                } else {
                    "".into()
                }
            });

            rv.push(if *need_wide_bar.borrow() {
                let total_width = state.width();
                let bar_width = total_width - measure_text_width(&s);
                s.replace("\x00", &self.format_bar(state, bar_width))
            } else {
                s.to_string()
            });
        }

        rv
    }
}

/// The state of a progress bar at a moment in time.
pub struct ProgressState {
    style: ProgressStyle,
    draw_target: DrawTarget,
    width: Option<u16>,
    message: String,
    pos: u64,
    len: u64,
    tick: u64,
    status: Status,
}

impl ProgressState {
    /// Returns the character that should be drawn for the
    /// current spinner character.
    pub fn current_tick_char(&self) -> char {
        if self.is_finished() {
            self.style.get_final_tick_char()
        } else {
            self.style.get_tick_char(self.tick)
        }
    }

    /// Indicates that a spinner should be drawn.
    pub fn has_spinner(&self) -> bool {
        self.tick != !0
    }

    /// Indicates that a progress bar should be drawn.
    pub fn has_progress(&self) -> bool {
        self.len != !0
    }

    /// Indicates that the progress bar finished.
    pub fn is_finished(&self) -> bool {
        match self.status {
            Status::InProgress => false,
            Status::DoneVisible => true,
            Status::DoneHidden => true,
        }
    }

    /// Returns `false` if the progress bar should no longer be
    /// drawn.
    pub fn should_render(&self) -> bool {
        match self.status {
            Status::DoneHidden => false,
            _ => true,
        }
    }

    /// Returns the completion in percent
    pub fn percent(&self) -> f32 {
        if self.len == !0 {
            0.0
        } else {
            self.pos as f32 / self.len as f32
        }
    }

    /// Returns the position of the status bar as `(pos, len)` tuple.
    pub fn position(&self) -> (u64, u64) {
        (self.pos, self.len)
    }

    /// Returns the current message of the progress bar.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The entire draw width
    pub fn width(&self) -> usize {
        if let Some(width) = self.width {
            width as usize
        } else {
            Term::stdout().size().1 as usize
        }
    }
}

/// A progress bar or spinner.
pub struct ProgressBar {
    state: RwLock<ProgressState>,
}

impl ProgressBar {
    /// Creates a new progress bar with a given length.
    ///
    /// This progress bar by default draws directly to stdout.
    pub fn new(len: u64) -> ProgressBar {
        ProgressBar {
            state: RwLock::new(ProgressState {
                style: Default::default(),
                draw_target: DrawTarget::stdout(),
                width: None,
                message: "".into(),
                pos: 0,
                len: len,
                tick: !0,
                status: Status::InProgress,
            }),
        }
    }

    /// Creates a new spinner.
    ///
    /// This spinner by default draws directly to stdout.
    pub fn new_spinner() -> ProgressBar {
        let rv = ProgressBar::new(!0);
        rv.enable_spinner();
        rv
    }

    /// Overrides the stored style.
    pub fn set_style(&self, style: ProgressStyle) {
        self.state.write().style = style;
    }

    /// Enables the spinner.
    ///
    /// This is obviously enabled by default if you create a progress
    /// bar with `new_spinner` but optionally a spinner can be added
    /// to a progress bar itself.
    pub fn enable_spinner(&self) {
        let mut state = self.state.write();
        if state.tick == !0 {
            state.tick = 0;
        }
    }

    /// Disables a spinner.
    ///
    /// This should not be called if the progress bar is a spinner itself.
    pub fn disable_spinner(&self) {
        let mut state = self.state.write();
        if state.tick != !0 {
            state.tick = !0;
        }
    }

    /// Manually ticks the spinner or progress bar.
    ///
    /// This automatically happens on any other change to a progress bar.
    pub fn tick(&self) {
        self.update_and_draw(|mut state| {
            if state.tick == !0 {
                state.tick = 0;
            } else {
                state.tick += 1;
            }
        });
    }

    /// Advances the position of a progress bar by delta.
    pub fn inc(&self, delta: u64) {
        self.update_and_draw(|mut state| {
            state.pos += delta;
            if state.tick != !0 {
                state.tick += 1;
            }
        })
    }

    /// Sets the position of the progress bar.
    pub fn set_position(&self, pos: u64) {
        self.update_and_draw(|mut state| {
            state.pos = pos;
        })
    }

    /// Sets the length of the progress bar.
    pub fn set_length(&self, len: u64) {
        self.update_and_draw(|mut state| {
            state.len = len;
        })
    }

    /// Sets the current message of the progress bar.
    pub fn set_message(&self, msg: &str) {
        let msg = msg.to_string();
        self.update_and_draw(|mut state| {
            state.message = msg;
        })
    }

    /// Finishes the progress bar and sets a message.
    pub fn finish_with_message(&self, msg: &str) {
        let msg = msg.to_string();
        self.update_and_draw(|mut state| {
            state.message = msg;
            state.pos = state.len;
            state.status = Status::DoneVisible;
        });
    }

    /// Finishes the progress bar and completely clears it.
    pub fn finish_and_clear(&self) {
        self.update_and_draw(|mut state| {
            state.pos = state.len;
            state.status = Status::DoneHidden;
        });
    }

    /// Sets a different draw target for the progress bar.
    ///
    /// This can be used to draw the progress bar to stderr
    /// for instance:
    ///
    /// ```rust,no_run
    /// # use indicatif::{ProgressBar, DrawTarget};
    /// let pb = ProgressBar::new(100);
    /// pb.set_draw_target(DrawTarget::stderr());
    /// ```
    pub fn set_draw_target(&self, target: DrawTarget) {
        self.state.write().draw_target = target;
    }

    fn update_and_draw<F: FnOnce(&mut ProgressState)>(&self, f: F) {
        {
            let mut state = self.state.write();
            f(&mut state);
        }
        self.draw().ok();
    }

    fn draw(&self) -> io::Result<()> {
        let mut state = self.state.write();
        let draw_state = DrawState {
            lines: if state.should_render() {
                state.style.format_state(&*state)
            } else {
                vec![]
            },
            finished: state.is_finished(),
            force_draw: false,
            ts: Instant::now(),
        };
        state.draw_target.apply_draw_state(draw_state)
    }
}


/// Manages multiple progress bars from different threads.
pub struct MultiProgress {
    objects: usize,
    draw_target: DrawTarget,
    tx: Sender<(usize, DrawState)>,
    rx: Receiver<(usize, DrawState)>,
}

impl MultiProgress {
    /// Creates a new multi progress object that draws to stdout.
    pub fn new() -> MultiProgress {
        let (tx, rx) = channel();
        MultiProgress {
            objects: 0,
            draw_target: DrawTarget::stdout(),
            tx: tx,
            rx: rx,
        }
    }

    /// Sets a different draw target for the multiprogress bar.
    pub fn set_draw_target(&mut self, target: DrawTarget) {
        self.draw_target = target;
    }

    /// Adds a progress bar.
    ///
    /// The progress bar added will have the draw target changed to a
    /// remote draw target that is intercepted by the multi progress
    /// object.
    pub fn add(&mut self, bar: ProgressBar) -> ProgressBar {
        bar.set_draw_target(DrawTarget::Remote(self.objects,
                                               self.tx.clone()));
        self.objects += 1;
        bar
    }

    /// Waits for all progress bars to report that they are finished.
    ///
    /// You need to call this as this will request the draw instructions
    /// from the remote progress bars.  Not calling this will deadlock
    /// your program.
    pub fn join(self) -> io::Result<()> {
        self.join_impl(false)
    }

    /// Works like `join` but clears the progress bar in the end.
    pub fn join_and_clear(self) -> io::Result<()> {
        self.join_impl(true)
    }

    fn join_impl(mut self, clear: bool) -> io::Result<()> {
        if self.objects == 0 {
            return Ok(());
        }

        let mut outstanding = repeat(true).take(self.objects as usize).collect::<Vec<_>>();
        let mut draw_states: Vec<Option<DrawState>> = outstanding.iter().map(|_| None).collect();

        while outstanding.iter().any(|&x| x) {
            let (idx, draw_state) = self.rx.recv().unwrap();
            let ts = draw_state.ts;
            let force_draw = draw_state.finished || draw_state.force_draw;

            if draw_state.finished {
                outstanding[idx] = false;
            }
            draw_states[idx] = Some(draw_state);

            let mut lines = vec![];
            for draw_state_opt in draw_states.iter() {
                if let Some(ref draw_state) = *draw_state_opt {
                    lines.extend_from_slice(&draw_state.lines[..]);
                }
            }

            self.draw_target.apply_draw_state(DrawState {
                lines: lines,
                force_draw: force_draw,
                finished: !outstanding.iter().any(|&x| x),
                ts: ts,
            })?;
        }

        if clear {
            self.draw_target.apply_draw_state(DrawState {
                lines: vec![],
                finished: true,
                force_draw: true,
                ts: Instant::now(),
            })?;
        }

        Ok(())
    }
}

impl Drop for ProgressBar {
    fn drop(&mut self) {
        if self.state.read().is_finished() {
            return;
        }
        self.update_and_draw(|mut state| {
            state.status = Status::DoneHidden;
        });
    }
}
