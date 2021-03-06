use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crate::style::{ProgressFinish, ProgressStyle};
use crate::utils::{duration_to_secs, secs_to_duration, Estimate};
use crate::{ProgressBarIter, ProgressIterator};
use console::Term;

/// The drawn state of an element.
#[derive(Clone, Debug)]
struct ProgressDrawState {
    /// The lines to print (can contain ANSI codes)
    pub lines: Vec<String>,
    /// The number of lines that shouldn't be reaped by the next tick.
    pub orphan_lines: usize,
    /// True if the bar no longer needs drawing.
    pub finished: bool,
    /// True if drawing should be forced.
    pub force_draw: bool,
    /// True if we should move the cursor up when possible instead of clearing lines.
    pub move_cursor: bool,
}

#[derive(Debug)]
enum Status {
    InProgress,
    DoneVisible,
    DoneHidden,
}

enum ProgressDrawTargetKind {
    Term {
        term: Term,
        last_state: Option<ProgressDrawState>,
        rate: Option<Duration>,
        last_draw: Option<Instant>,
    },
    Remote {
        state: Arc<RwLock<MultiProgressState>>,
        idx: usize,
        chan: Mutex<Sender<(usize, ProgressDrawState)>>,
    },
    Hidden,
}

/// Target for draw operations
///
/// This tells a progress bar or a multi progress object where to paint to.
/// The draw target is a stateful wrapper over a drawing destination and
/// internally optimizes how often the state is painted to the output
/// device.
pub struct ProgressDrawTarget {
    kind: ProgressDrawTargetKind,
}

impl ProgressDrawTarget {
    /// Draw to a buffered stdout terminal at a max of 15 times a second.
    ///
    /// For more information see `ProgressDrawTarget::to_term`.
    pub fn stdout() -> ProgressDrawTarget {
        ProgressDrawTarget::term(Term::buffered_stdout(), 15)
    }

    /// Draw to a buffered stderr terminal at a max of 15 times a second.
    ///
    /// This is the default draw target for progress bars.  For more
    /// information see `ProgressDrawTarget::to_term`.
    pub fn stderr() -> ProgressDrawTarget {
        ProgressDrawTarget::term(Term::buffered_stderr(), 15)
    }

    /// Draw to a buffered stdout terminal at a max of `refresh_rate` times a second.
    ///
    /// For more information see `ProgressDrawTarget::to_term`.
    pub fn stdout_with_hz(refresh_rate: u64) -> ProgressDrawTarget {
        ProgressDrawTarget::term(Term::buffered_stdout(), refresh_rate)
    }

    /// Draw to a buffered stderr terminal at a max of `refresh_rate` times a second.
    ///
    /// For more information see `ProgressDrawTarget::to_term`.
    pub fn stderr_with_hz(refresh_rate: u64) -> ProgressDrawTarget {
        ProgressDrawTarget::term(Term::buffered_stderr(), refresh_rate)
    }

    /// Draw to a buffered stdout terminal without max framerate.
    ///
    /// This is useful when data is known to come in very slowly and
    /// not rendering some updates would be a problem (for instance
    /// when messages are used extensively).
    ///
    /// For more information see `ProgressDrawTarget::to_term`.
    pub fn stdout_nohz() -> ProgressDrawTarget {
        ProgressDrawTarget::term(Term::buffered_stdout(), None)
    }

    /// Draw to a buffered stderr terminal without max framerate.
    ///
    /// This is useful when data is known to come in very slowly and
    /// not rendering some updates would be a problem (for instance
    /// when messages are used extensively).
    ///
    /// For more information see `ProgressDrawTarget::to_term`.
    pub fn stderr_nohz() -> ProgressDrawTarget {
        ProgressDrawTarget::term(Term::buffered_stderr(), None)
    }

    /// Draw to a terminal, optionally with a specific refresh rate.
    ///
    /// Progress bars are by default drawn to terminals however if the
    /// terminal is not user attended the entire progress bar will be
    /// hidden.  This is done so that piping to a file will not produce
    /// useless escape codes in that file.
    ///
    /// Will panic if refresh_rate is `Some(0)`. To disable rate limiting use `None` instead.
    #[allow(clippy::wrong_self_convention)]
    #[deprecated(since = "0.16.0", note = "Use `ProgressDrawTarget::term` instead")]
    pub fn to_term(term: Term, refresh_rate: impl Into<Option<u64>>) -> ProgressDrawTarget {
        ProgressDrawTarget::term(term, refresh_rate)
    }

    /// Draw to a terminal, optionally with a specific refresh rate.
    ///
    /// Progress bars are by default drawn to terminals however if the
    /// terminal is not user attended the entire progress bar will be
    /// hidden.  This is done so that piping to a file will not produce
    /// useless escape codes in that file.
    ///
    /// Will panic if refresh_rate is `Some(0)`. To disable rate limiting use `None` instead.
    pub fn term(term: Term, refresh_rate: impl Into<Option<u64>>) -> ProgressDrawTarget {
        let rate = refresh_rate.into().map(|x| Duration::from_millis(1000 / x));
        ProgressDrawTarget {
            kind: ProgressDrawTargetKind::Term {
                term,
                last_state: None,
                rate,
                last_draw: None,
            },
        }
    }

    /// A hidden draw target.
    ///
    /// This forces a progress bar to be not rendered at all.
    pub fn hidden() -> ProgressDrawTarget {
        ProgressDrawTarget {
            kind: ProgressDrawTargetKind::Hidden,
        }
    }

    /// Returns true if the draw target is hidden.
    ///
    /// This is internally used in progress bars to figure out if overhead
    /// from drawing can be prevented.
    pub fn is_hidden(&self) -> bool {
        match self.kind {
            ProgressDrawTargetKind::Hidden => true,
            ProgressDrawTargetKind::Term { ref term, .. } => !term.is_term(),
            _ => false,
        }
    }

    /// Returns the current width of the draw target.
    fn width(&self) -> usize {
        match self.kind {
            ProgressDrawTargetKind::Term { ref term, .. } => term.size().1 as usize,
            ProgressDrawTargetKind::Remote { ref state, .. } => state.read().unwrap().width(),
            ProgressDrawTargetKind::Hidden => unreachable!(),
        }
    }

    /// Apply the given draw state (draws it).
    fn apply_draw_state(&mut self, draw_state: ProgressDrawState) -> io::Result<()> {
        // no need to apply anything to hidden draw targets.
        if self.is_hidden() {
            return Ok(());
        }
        match self.kind {
            ProgressDrawTargetKind::Term {
                ref term,
                ref mut last_state,
                rate,
                ref mut last_draw,
            } => {
                if draw_state.finished
                    || draw_state.force_draw
                    || rate.is_none()
                    || last_draw.is_none()
                    || last_draw.unwrap().elapsed() > rate.unwrap()
                {
                    if let Some(ref last_state) = *last_state {
                        if !draw_state.lines.is_empty() && draw_state.move_cursor {
                            last_state.move_cursor(term)?;
                        } else {
                            last_state.clear_term(term)?;
                        }
                    }
                    draw_state.draw_to_term(term)?;
                    term.flush()?;
                    *last_state = Some(draw_state);
                    *last_draw = Some(Instant::now());
                }
            }
            ProgressDrawTargetKind::Remote { idx, ref chan, .. } => {
                return chan
                    .lock()
                    .unwrap()
                    .send((idx, draw_state))
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e));
            }
            ProgressDrawTargetKind::Hidden => {}
        }
        Ok(())
    }

    /// Properly disconnects from the draw target
    fn disconnect(&self) {
        match self.kind {
            ProgressDrawTargetKind::Term { .. } => {}
            ProgressDrawTargetKind::Remote { idx, ref chan, .. } => {
                chan.lock()
                    .unwrap()
                    .send((
                        idx,
                        ProgressDrawState {
                            lines: vec![],
                            orphan_lines: 0,
                            finished: true,
                            force_draw: false,
                            move_cursor: false,
                        },
                    ))
                    .ok();
            }
            ProgressDrawTargetKind::Hidden => {}
        };
    }
}

impl ProgressDrawState {
    pub fn clear_term(&self, term: &Term) -> io::Result<()> {
        term.clear_last_lines(self.lines.len() - self.orphan_lines)
    }

    pub fn move_cursor(&self, term: &Term) -> io::Result<()> {
        term.move_cursor_up(self.lines.len() - self.orphan_lines)
    }

    pub fn draw_to_term(&self, term: &Term) -> io::Result<()> {
        for line in &self.lines {
            term.write_line(line)?;
        }
        Ok(())
    }
}

/// The state of a progress bar at a moment in time.
pub(crate) struct ProgressState {
    pub(crate) style: ProgressStyle,
    pub(crate) pos: u64,
    pub(crate) len: u64,
    pub(crate) tick: u64,
    pub(crate) started: Instant,
    draw_target: ProgressDrawTarget,
    message: String,
    prefix: String,
    draw_delta: u64,
    draw_rate: u64,
    draw_next: u64,
    status: Status,
    est: Estimate,
    tick_thread: Option<thread::JoinHandle<()>>,
    steady_tick: u64,
}

impl ProgressState {
    /// Returns the string that should be drawn for the
    /// current spinner string.
    pub fn current_tick_str(&self) -> &str {
        if self.is_finished() {
            self.style.get_final_tick_str()
        } else {
            self.style.get_tick_str(self.tick)
        }
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
        !matches!(self.status, Status::DoneHidden)
    }

    /// Returns the completion as a floating-point number between 0 and 1
    pub fn fraction(&self) -> f32 {
        let pct = match (self.pos, self.len) {
            (_, 0) => 1.0,
            (0, _) => 0.0,
            (pos, len) => pos as f32 / len as f32,
        };
        pct.max(0.0).min(1.0)
    }

    /// Returns the position of the status bar as `(pos, len)` tuple.
    pub fn position(&self) -> (u64, u64) {
        (self.pos, self.len)
    }

    /// Returns the current message of the progress bar.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the current prefix of the progress bar.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The entire draw width
    pub fn width(&self) -> usize {
        self.draw_target.width()
    }

    /// Return the current average time per step
    pub fn avg_time_per_step(&self) -> Duration {
        self.est.time_per_step()
    }

    /// The expected ETA
    pub fn eta(&self) -> Duration {
        if self.len == !0 || self.is_finished() {
            return Duration::new(0, 0);
        }
        let t = duration_to_secs(self.avg_time_per_step());
        // add 0.75 to leave 0.25 sec of 0s for the user
        secs_to_duration(t * self.len.saturating_sub(self.pos) as f64 + 0.75)
    }

    /// The expected total duration (that is, elapsed time + expected ETA)
    pub fn duration(&self) -> Duration {
        if self.len == !0 || self.is_finished() {
            return Duration::new(0, 0);
        }
        self.started.elapsed() + self.eta()
    }

    /// The number of steps per second
    pub fn per_sec(&self) -> u64 {
        let avg_time = self.avg_time_per_step().as_nanos();
        if avg_time == 0 {
            0
        } else {
            (1_000_000_000 / avg_time) as u64
        }
    }

    /// Call the provided `FnOnce` to update the state.  Then redraw the
    /// progress bar if the state has changed.
    pub fn update_and_draw<F: FnOnce(&mut ProgressState)>(&mut self, f: F) {
        let mut draw = false;
        {
            let old_pos = self.pos;
            f(self);
            let new_pos = self.pos;
            if new_pos != old_pos {
                self.est.record_step(new_pos);
            }
            if new_pos >= self.draw_next {
                self.draw_next = new_pos.saturating_add(if self.draw_rate != 0 {
                    self.per_sec() / self.draw_rate
                } else {
                    self.draw_delta
                });
                draw = true;
            }
        }
        if draw {
            draw_state(self).ok();
        }
    }

    /// Finishes the progress bar and leaves the current message.
    pub fn finish(&mut self) {
        self.update_and_draw(|state| {
            state.pos = state.len;
            state.draw_next = state.pos;
            state.status = Status::DoneVisible;
        });
    }

    /// Finishes the progress bar at current position and leaves the current message.
    pub fn finish_at_current_pos(&mut self) {
        self.update_and_draw(|state| {
            state.draw_next = state.pos;
            state.status = Status::DoneVisible;
        });
    }

    /// Finishes the progress bar and sets a message.
    pub fn finish_with_message(&mut self, msg: &str) {
        let msg = msg.to_string();
        self.update_and_draw(|state| {
            state.message = msg;
            state.pos = state.len;
            state.draw_next = state.pos;
            state.status = Status::DoneVisible;
        });
    }

    /// Finishes the progress bar and completely clears it.
    pub fn finish_and_clear(&mut self) {
        self.update_and_draw(|state| {
            state.pos = state.len;
            state.draw_next = state.pos;
            state.status = Status::DoneHidden;
        });
    }

    /// Finishes the progress bar and leaves the current message and progress.
    pub fn abandon(&mut self) {
        self.update_and_draw(|state| {
            state.status = Status::DoneVisible;
        });
    }

    /// Finishes the progress bar and sets a message, and leaves the current progress.
    pub fn abandon_with_message(&mut self, msg: &str) {
        let msg = msg.to_string();
        self.update_and_draw(|state| {
            state.message = msg;
            state.status = Status::DoneVisible;
        });
    }

    /// Finishes the progress bar using the [`ProgressFinish`] behavior stored
    /// in the [`ProgressStyle`].
    pub fn finish_using_style(&mut self) {
        if let Some(on_finish) = std::mem::take(&mut self.style.on_finish) {
            match on_finish {
                ProgressFinish::Default => {
                    self.finish();
                }
                ProgressFinish::AtCurrentPos => {
                    self.finish_at_current_pos();
                }
                ProgressFinish::WithMessage(msg) => {
                    self.finish_with_message(&msg);
                }
                ProgressFinish::AndClear => {
                    self.finish_and_clear();
                }
                ProgressFinish::Abandon => {
                    self.abandon();
                }
                ProgressFinish::AbandonWithMessage(msg) => {
                    self.abandon_with_message(&msg);
                }
            }
        }
    }
}

/// A progress bar or spinner.
///
/// The progress bar is an `Arc` around an internal state.  When the progress
/// bar is cloned it just increments the refcount which means the bar is
/// shared with the original one.
#[derive(Clone)]
pub struct ProgressBar {
    state: Arc<Mutex<ProgressState>>,
}

impl fmt::Debug for ProgressBar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgressBar").finish()
    }
}

impl ProgressBar {
    /// Creates a new progress bar with a given length.
    ///
    /// This progress bar by default draws directly to stderr, and refreshes
    /// a maximum of 15 times a second. To change the refresh rate set the
    /// draw target to one with a different refresh rate.
    pub fn new(len: u64) -> ProgressBar {
        ProgressBar::with_draw_target(len, ProgressDrawTarget::stderr())
    }

    /// Creates a completely hidden progress bar.
    ///
    /// This progress bar still responds to API changes but it does not
    /// have a length or render in any way.
    pub fn hidden() -> ProgressBar {
        ProgressBar::with_draw_target(!0, ProgressDrawTarget::hidden())
    }

    /// Creates a new progress bar with a given length and draw target.
    pub fn with_draw_target(len: u64, target: ProgressDrawTarget) -> ProgressBar {
        ProgressBar {
            state: Arc::new(Mutex::new(ProgressState {
                style: ProgressStyle::default_bar(),
                draw_target: target,
                message: "".into(),
                prefix: "".into(),
                pos: 0,
                len,
                tick: 0,
                draw_delta: 0,
                draw_rate: 0,
                draw_next: 0,
                status: Status::InProgress,
                started: Instant::now(),
                est: Estimate::new(),
                tick_thread: None,
                steady_tick: 0,
            })),
        }
    }

    /// A convenience builder-like function for a progress bar with a given style.
    pub fn with_style(self, style: ProgressStyle) -> ProgressBar {
        self.state.lock().unwrap().style = style;
        self
    }

    /// A convenience builder-like function for a progress bar with a given prefix.
    pub fn with_prefix(self, prefix: &str) -> ProgressBar {
        self.state.lock().unwrap().prefix = prefix.to_string();
        self
    }

    /// A convenience builder-like function for a progress bar with a given message.
    pub fn with_message(self, message: &str) -> ProgressBar {
        self.state.lock().unwrap().message = message.to_string();
        self
    }

    /// A convenience builder-like function for a progress bar with a given position.
    pub fn with_position(self, pos: u64) -> ProgressBar {
        self.state.lock().unwrap().pos = pos;
        self
    }

    /// Creates a new spinner.
    ///
    /// This spinner by default draws directly to stderr.  This adds the
    /// default spinner style to it.
    pub fn new_spinner() -> ProgressBar {
        let rv = ProgressBar::new(!0);
        rv.set_style(ProgressStyle::default_spinner());
        rv
    }

    /// Overrides the stored style.
    ///
    /// This does not redraw the bar.  Call `tick` to force it.
    pub fn set_style(&self, style: ProgressStyle) {
        self.state.lock().unwrap().style = style;
    }

    /// Spawns a background thread to tick the progress bar.
    ///
    /// When this is enabled a background thread will regularly tick the
    /// progress back in the given interval (milliseconds).  This is
    /// useful to advance progress bars that are very slow by themselves.
    ///
    /// When steady ticks are enabled calling `.tick()` on a progress
    /// bar does not do anything.
    pub fn enable_steady_tick(&self, ms: u64) {
        let mut state = self.state.lock().unwrap();
        state.steady_tick = ms;
        if state.tick_thread.is_some() {
            return;
        }

        // Using a weak pointer is required to prevent a potential deadlock. See issue #133
        let state_arc = Arc::downgrade(&self.state);
        state.tick_thread = Some(thread::spawn(move || Self::steady_tick(state_arc, ms)));

        ::std::mem::drop(state);
        // use the side effect of tick to force the bar to tick.
        self.tick();
    }

    fn steady_tick(state_arc: Weak<Mutex<ProgressState>>, mut ms: u64) {
        loop {
            thread::sleep(Duration::from_millis(ms));
            if let Some(state_arc) = state_arc.upgrade() {
                let mut state = state_arc.lock().unwrap();
                if state.is_finished() || state.steady_tick == 0 {
                    state.steady_tick = 0;
                    state.tick_thread = None;
                    break;
                }
                if state.tick != 0 {
                    state.tick = state.tick.saturating_add(1);
                }
                ms = state.steady_tick;

                draw_state(&mut state).ok();
            } else {
                break;
            }
        }
    }

    /// Undoes `enable_steady_tick`.
    pub fn disable_steady_tick(&self) {
        self.enable_steady_tick(0);
    }

    /// Limit redrawing of progress bar to every `n` steps. Defaults to 0.
    ///
    /// By default, the progress bar will redraw whenever its state advances.
    /// This setting is helpful in situations where the overhead of redrawing
    /// the progress bar dominates the computation whose progress is being
    /// reported.
    ///
    /// If `n` is greater than 0, operations that change the progress bar such
    /// as `.tick()`, `.set_message()` and `.set_length()` will no longer cause
    /// the progress bar to be redrawn, and will only be shown once the
    /// position advances by `n` steps.
    ///
    /// ```rust,no_run
    /// # use indicatif::ProgressBar;
    /// let n = 1_000_000;
    /// let pb = ProgressBar::new(n);
    /// pb.set_draw_delta(n / 100); // redraw every 1% of additional progress
    /// ```
    ///
    /// Note that `ProgressDrawTarget` may impose additional buffering of redraws.
    pub fn set_draw_delta(&self, n: u64) {
        let mut state = self.state.lock().unwrap();
        state.draw_delta = n;
        state.draw_next = state.pos.saturating_add(state.draw_delta);
    }

    /// Sets the refresh rate of progress bar to `n` updates per seconds. Defaults to 0.
    ///
    /// This is similar to `set_draw_delta` but automatically adapts to a constant refresh rate
    /// regardless of how consistent the progress is.
    ///
    /// This parameter takes precedence on `set_draw_delta` if different from 0.
    ///
    /// ```rust,no_run
    /// # use indicatif::ProgressBar;
    /// let n = 1_000_000;
    /// let pb = ProgressBar::new(n);
    /// pb.set_draw_rate(25); // aims at redrawing at most 25 times per seconds.
    /// ```
    ///
    /// Note that `ProgressDrawTarget` may impose additional buffering of redraws.
    pub fn set_draw_rate(&self, n: u64) {
        let mut state = self.state.lock().unwrap();
        state.draw_rate = n;
        state.draw_next = state.pos.saturating_add(state.per_sec() / n);
    }

    /// Manually ticks the spinner or progress bar.
    ///
    /// This automatically happens on any other change to a progress bar.
    pub fn tick(&self) {
        self.update_and_draw(|state| {
            if state.steady_tick == 0 || state.tick == 0 {
                state.tick = state.tick.saturating_add(1);
            }
        });
    }

    /// Advances the position of a progress bar by delta.
    pub fn inc(&self, delta: u64) {
        self.update_and_draw(|state| {
            state.pos = state.pos.saturating_add(delta);
            if state.steady_tick == 0 || state.tick == 0 {
                state.tick = state.tick.saturating_add(1);
            }
        })
    }

    /// A quick convenience check if the progress bar is hidden.
    pub fn is_hidden(&self) -> bool {
        self.state.lock().unwrap().draw_target.is_hidden()
    }

    /// Indicates that the progress bar finished.
    pub fn is_finished(&self) -> bool {
        self.state.lock().unwrap().is_finished()
    }

    /// Print a log line above the progress bar.
    ///
    /// If the progress bar was added to a `MultiProgress`, the log line will be
    /// printed above all other progress bars.
    ///
    /// Note that if the progress bar is hidden (which by default happens if
    /// the progress bar is redirected into a file) println will not do
    /// anything either.
    pub fn println<I: AsRef<str>>(&self, msg: I) {
        let mut state = self.state.lock().unwrap();

        let mut lines: Vec<String> = msg.as_ref().lines().map(Into::into).collect();
        let orphan_lines = lines.len();
        if state.should_render() && !state.draw_target.is_hidden() {
            lines.extend(state.style.format_state(&*state));
        }

        let draw_state = ProgressDrawState {
            lines,
            orphan_lines,
            finished: state.is_finished(),
            force_draw: true,
            move_cursor: false,
        };

        state.draw_target.apply_draw_state(draw_state).ok();
    }

    /// Sets the position of the progress bar.
    pub fn set_position(&self, pos: u64) {
        self.update_and_draw(|state| {
            state.draw_next = pos;
            state.pos = pos;
            if state.steady_tick == 0 || state.tick == 0 {
                state.tick = state.tick.saturating_add(1);
            }
        })
    }

    /// Sets the length of the progress bar.
    pub fn set_length(&self, len: u64) {
        self.update_and_draw(|state| {
            state.len = len;
        })
    }

    /// Increase the length of the progress bar.
    pub fn inc_length(&self, delta: u64) {
        self.update_and_draw(|state| {
            state.len = state.len.saturating_add(delta);
        })
    }

    /// Sets the current prefix of the progress bar.
    ///
    /// For the prefix to be visible, `{prefix}` placeholder
    /// must be present in the template (see `ProgressStyle`).
    pub fn set_prefix(&self, prefix: &str) {
        let prefix = prefix.to_string();
        self.update_and_draw(|state| {
            state.prefix = prefix;
            if state.steady_tick == 0 || state.tick == 0 {
                state.tick = state.tick.saturating_add(1);
            }
        })
    }

    /// Sets the current message of the progress bar.
    ///
    /// For the message to be visible, `{msg}` placeholder
    /// must be present in the template (see `ProgressStyle`).
    pub fn set_message(&self, msg: &str) {
        let msg = msg.to_string();
        self.update_and_draw(|state| {
            state.message = msg;
            if state.steady_tick == 0 || state.tick == 0 {
                state.tick = state.tick.saturating_add(1);
            }
        })
    }

    /// Creates a new weak reference to this `ProgressBar`.
    pub fn downgrade(&self) -> WeakProgressBar {
        WeakProgressBar {
            state: Arc::downgrade(&self.state),
        }
    }

    /// Resets the ETA calculation.
    ///
    /// This can be useful if progress bars make a huge jump or were
    /// paused for a prolonged time.
    pub fn reset_eta(&self) {
        self.update_and_draw(|state| {
            state.est.reset(state.pos);
        });
    }

    /// Resets elapsed time
    pub fn reset_elapsed(&self) {
        self.update_and_draw(|state| {
            state.started = Instant::now();
        });
    }

    pub fn reset(&self) {
        self.reset_eta();
        self.reset_elapsed();
        self.update_and_draw(|state| {
            state.draw_next = 0;
            state.pos = 0;
            state.status = Status::InProgress;
        });
    }

    /// Finishes the progress bar and leaves the current message.
    pub fn finish(&self) {
        self.state.lock().unwrap().finish();
    }

    /// Finishes the progress bar at current position and leaves the current message.
    pub fn finish_at_current_pos(&self) {
        self.state.lock().unwrap().finish_at_current_pos();
    }

    /// Finishes the progress bar and sets a message.
    ///
    /// For the message to be visible, `{msg}` placeholder
    /// must be present in the template (see `ProgressStyle`).
    pub fn finish_with_message(&self, msg: &str) {
        self.state.lock().unwrap().finish_with_message(msg);
    }

    /// Finishes the progress bar and completely clears it.
    pub fn finish_and_clear(&self) {
        self.state.lock().unwrap().finish_and_clear();
    }

    /// Finishes the progress bar and leaves the current message and progress.
    pub fn abandon(&self) {
        self.state.lock().unwrap().abandon();
    }

    /// Finishes the progress bar and sets a message, and leaves the current progress.
    ///
    /// For the message to be visible, `{msg}` placeholder
    /// must be present in the template (see `ProgressStyle`).
    pub fn abandon_with_message(&self, msg: &str) {
        self.state.lock().unwrap().abandon_with_message(msg);
    }

    /// Finishes the progress bar using the [`ProgressFinish`] behavior stored
    /// in the [`ProgressStyle`].
    pub fn finish_using_style(&self) {
        self.state.lock().unwrap().finish_using_style();
    }

    /// Sets a different draw target for the progress bar.
    ///
    /// This can be used to draw the progress bar to stderr
    /// for instance:
    ///
    /// ```rust,no_run
    /// # use indicatif::{ProgressBar, ProgressDrawTarget};
    /// let pb = ProgressBar::new(100);
    /// pb.set_draw_target(ProgressDrawTarget::stderr());
    /// ```
    pub fn set_draw_target(&self, target: ProgressDrawTarget) {
        let mut state = self.state.lock().unwrap();
        state.draw_target.disconnect();
        state.draw_target = target;
    }

    /// Wraps an iterator with the progress bar.
    ///
    /// ```rust,no_run
    /// # use indicatif::ProgressBar;
    /// let v = vec![1, 2, 3];
    /// let pb = ProgressBar::new(3);
    /// for item in pb.wrap_iter(v.iter()) {
    ///     // ...
    /// }
    /// ```
    pub fn wrap_iter<It: Iterator>(&self, it: It) -> ProgressBarIter<It> {
        it.progress_with(self.clone())
    }

    /// Wraps a Reader with the progress bar.
    ///
    /// ```rust,no_run
    /// # use std::fs::File;
    /// # use std::io;
    /// # use indicatif::ProgressBar;
    /// # fn test () -> io::Result<()> {
    /// let source = File::open("work.txt")?;
    /// let mut target = File::create("done.txt")?;
    /// let pb = ProgressBar::new(source.metadata()?.len());
    /// io::copy(&mut pb.wrap_read(source), &mut target);
    /// # Ok(())
    /// # }
    /// ```
    pub fn wrap_read<R: io::Read>(&self, read: R) -> ProgressBarIter<R> {
        ProgressBarIter {
            progress: self.clone(),
            it: read,
        }
    }

    /// Wraps a Writer with the progress bar.
    ///
    /// ```rust,no_run
    /// # use std::fs::File;
    /// # use std::io;
    /// # use indicatif::ProgressBar;
    /// # fn test () -> io::Result<()> {
    /// let mut source = File::open("work.txt")?;
    /// let target = File::create("done.txt")?;
    /// let pb = ProgressBar::new(source.metadata()?.len());
    /// io::copy(&mut source, &mut pb.wrap_write(target));
    /// # Ok(())
    /// # }
    /// ```
    pub fn wrap_write<W: io::Write>(&self, write: W) -> ProgressBarIter<W> {
        ProgressBarIter {
            progress: self.clone(),
            it: write,
        }
    }

    fn update_and_draw<F: FnOnce(&mut ProgressState)>(&self, f: F) {
        // Delegate to the wrapped state.
        let mut state = self.state.lock().unwrap();
        state.update_and_draw(f);
    }

    pub fn position(&self) -> u64 {
        self.state.lock().unwrap().pos
    }

    pub fn length(&self) -> u64 {
        self.state.lock().unwrap().len
    }
}

fn draw_state(state: &mut ProgressState) -> io::Result<()> {
    // we can bail early if the draw target is hidden.
    if state.draw_target.is_hidden() {
        return Ok(());
    }

    let draw_state = ProgressDrawState {
        lines: if state.should_render() {
            state.style.format_state(&*state)
        } else {
            vec![]
        },
        orphan_lines: 0,
        finished: state.is_finished(),
        force_draw: false,
        move_cursor: false,
    };
    state.draw_target.apply_draw_state(draw_state)
}

/// A weak reference to a `ProgressBar`.
///
/// Useful for creating custom steady tick implementations
#[derive(Clone)]
pub struct WeakProgressBar {
    state: Weak<Mutex<ProgressState>>,
}

impl WeakProgressBar {
    /// Attempts to upgrade the Weak pointer to a [`ProgressBar`], delaying dropping of the inner
    /// value if successful. Returns `None` if the inner value has since been dropped.
    ///
    /// [`ProgressBar`]: struct.ProgressBar.html
    pub fn upgrade(&self) -> Option<ProgressBar> {
        self.state.upgrade().map(|state| ProgressBar { state })
    }
}

impl Drop for ProgressState {
    fn drop(&mut self) {
        // Progress bar is already finished.  Do not need to do anything.
        if self.is_finished() {
            return;
        }

        // How should we finish the bar?
        match std::mem::take(&mut self.style.on_finish) {
            Some(on_finish) => match on_finish {
                ProgressFinish::Default => {
                    self.finish();
                }
                ProgressFinish::AtCurrentPos => {
                    self.finish_at_current_pos();
                }
                ProgressFinish::WithMessage(msg) => {
                    self.finish_with_message(&msg);
                }
                ProgressFinish::AndClear => {
                    self.finish_and_clear();
                }
                ProgressFinish::Abandon => {
                    self.abandon();
                }
                ProgressFinish::AbandonWithMessage(msg) => {
                    self.abandon_with_message(&msg);
                }
            },
            // Fallback to original drop behavior for bars that are not finished.
            None => {
                self.status = Status::DoneHidden;
                draw_state(self).ok();
            }
        }
    }
}

#[allow(clippy::float_cmp)]
#[test]
fn test_pbar_zero() {
    let pb = ProgressBar::new(0);
    assert_eq!(pb.state.lock().unwrap().fraction(), 1.0);
}

#[allow(clippy::float_cmp)]
#[test]
fn test_pbar_maxu64() {
    let pb = ProgressBar::new(!0);
    assert_eq!(pb.state.lock().unwrap().fraction(), 0.0);
}

#[test]
fn test_pbar_overflow() {
    let pb = ProgressBar::new(1);
    pb.set_draw_target(ProgressDrawTarget::hidden());
    pb.inc(2);
    pb.finish();
}

#[test]
fn test_get_position() {
    let pb = ProgressBar::new(1);
    pb.set_draw_target(ProgressDrawTarget::hidden());
    pb.inc(2);
    let pos = pb.position();
    assert_eq!(pos, 2);
}

#[test]
fn test_weak_pb() {
    let pb = ProgressBar::new(0);
    let weak = pb.downgrade();
    assert!(weak.upgrade().is_some());
    ::std::mem::drop(pb);
    assert!(weak.upgrade().is_none());
}

#[test]
fn test_draw_delta_deadlock() {
    // see issue #187
    let mpb = MultiProgress::new();
    let pb = mpb.add(ProgressBar::new(1));
    pb.set_draw_delta(2);
    drop(pb);
    mpb.join().unwrap();
}

struct MultiObject {
    done: bool,
    draw_state: Option<ProgressDrawState>,
}

struct MultiProgressState {
    objects: Vec<MultiObject>,
    ordering: Vec<usize>,
    draw_target: ProgressDrawTarget,
    move_cursor: bool,
}

impl MultiProgressState {
    fn width(&self) -> usize {
        self.draw_target.width()
    }

    fn is_done(&self) -> bool {
        if self.objects.is_empty() {
            return true;
        }
        for obj in &self.objects {
            if !obj.done {
                return false;
            }
        }
        true
    }
}

/// Manages multiple progress bars from different threads.
pub struct MultiProgress {
    state: Arc<RwLock<MultiProgressState>>,
    joining: AtomicBool,
    tx: Sender<(usize, ProgressDrawState)>,
    rx: Receiver<(usize, ProgressDrawState)>,
}

impl fmt::Debug for MultiProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MultiProgress").finish()
    }
}

unsafe impl Sync for MultiProgress {}

impl Default for MultiProgress {
    fn default() -> MultiProgress {
        MultiProgress::with_draw_target(ProgressDrawTarget::stderr())
    }
}

impl MultiProgress {
    /// Creates a new multi progress object.
    ///
    /// Progress bars added to this object by default draw directly to stderr, and refresh
    /// a maximum of 15 times a second. To change the refresh rate set the draw target to
    /// one with a different refresh rate.
    pub fn new() -> MultiProgress {
        MultiProgress::default()
    }

    /// Creates a new multi progress object with the given draw target.
    pub fn with_draw_target(draw_target: ProgressDrawTarget) -> MultiProgress {
        let (tx, rx) = channel();
        MultiProgress {
            state: Arc::new(RwLock::new(MultiProgressState {
                objects: vec![],
                ordering: vec![],
                draw_target,
                move_cursor: false,
            })),
            joining: AtomicBool::new(false),
            tx,
            rx,
        }
    }

    /// Sets a different draw target for the multiprogress bar.
    pub fn set_draw_target(&self, target: ProgressDrawTarget) {
        let mut state = self.state.write().unwrap();
        state.draw_target.disconnect();
        state.draw_target = target;
    }

    /// Set whether we should try to move the cursor when possible instead of clearing lines.
    ///
    /// This can reduce flickering, but do not enable it if you intend to change the number of
    /// progress bars.
    pub fn set_move_cursor(&self, move_cursor: bool) {
        self.state.write().unwrap().move_cursor = move_cursor;
    }

    /// Adds a progress bar.
    ///
    /// The progress bar added will have the draw target changed to a
    /// remote draw target that is intercepted by the multi progress
    /// object overriding custom `ProgressDrawTarget` settings.
    pub fn add(&self, pb: ProgressBar) -> ProgressBar {
        let mut state = self.state.write().unwrap();
        let idx = state.objects.len();
        state.objects.push(MultiObject {
            done: false,
            draw_state: None,
        });
        state.ordering.push(idx);
        pb.set_draw_target(ProgressDrawTarget {
            kind: ProgressDrawTargetKind::Remote {
                state: self.state.clone(),
                idx,
                chan: Mutex::new(self.tx.clone()),
            },
        });
        pb
    }

    /// Inserts a progress bar.
    ///
    /// The progress bar inserted at position `index` will have the draw
    /// target changed to a remote draw target that is intercepted by the
    /// multi progress object overriding custom `ProgressDrawTarget` settings.
    ///
    /// If `index >= MultiProgressState::objects.len()`, the progress bar
    /// is added to the end of the list.
    pub fn insert(&self, index: usize, pb: ProgressBar) -> ProgressBar {
        let mut state = self.state.write().unwrap();
        let object_idx = state.objects.len();
        state.objects.push(MultiObject {
            done: false,
            draw_state: None,
        });
        if index > state.ordering.len() {
            state.ordering.push(object_idx);
        } else {
            state.ordering.insert(index, object_idx);
        }
        pb.set_draw_target(ProgressDrawTarget {
            kind: ProgressDrawTargetKind::Remote {
                state: self.state.clone(),
                idx: object_idx,
                chan: Mutex::new(self.tx.clone()),
            },
        });
        pb
    }

    /// Waits for all progress bars to report that they are finished.
    ///
    /// You need to call this as this will request the draw instructions
    /// from the remote progress bars.  Not calling this will deadlock
    /// your program.
    pub fn join(&self) -> io::Result<()> {
        self.join_impl(false)
    }

    /// Works like `join` but clears the progress bar in the end.
    pub fn join_and_clear(&self) -> io::Result<()> {
        self.join_impl(true)
    }

    fn join_impl(&self, clear: bool) -> io::Result<()> {
        if self.joining.load(Ordering::Acquire) {
            panic!("Already joining!");
        }
        self.joining.store(true, Ordering::Release);

        let move_cursor = self.state.read().unwrap().move_cursor;
        // Max amount of grouped together updates at once. This is meant
        // to ensure there isn't a situation where continuous updates prevent
        // any actual draws happening.
        const MAX_GROUP_SIZE: usize = 32;
        let mut recv_peek = None;
        let mut grouped = 0usize;
        let mut orphan_lines: Vec<String> = Vec::new();
        let mut force_draw = false;
        while !self.state.read().unwrap().is_done() {
            let (idx, draw_state) = if let Some(peeked) = recv_peek.take() {
                peeked
            } else {
                self.rx.recv().unwrap()
            };
            force_draw |= draw_state.finished || draw_state.force_draw;

            let mut state = self.state.write().unwrap();
            if draw_state.finished {
                state.objects[idx].done = true;
            }

            // Split orphan lines out of the draw state, if any
            let lines = if draw_state.orphan_lines > 0 {
                let split = draw_state.lines.split_at(draw_state.orphan_lines);
                orphan_lines.extend_from_slice(split.0);
                split.1.to_vec()
            } else {
                draw_state.lines
            };

            let draw_state = ProgressDrawState {
                lines,
                orphan_lines: 0,
                ..draw_state
            };

            state.objects[idx].draw_state = Some(draw_state);

            // the rest from here is only drawing, we can skip it.
            if state.draw_target.is_hidden() {
                continue;
            }

            debug_assert!(recv_peek.is_none());
            if grouped >= MAX_GROUP_SIZE {
                // Can't group any more draw calls, proceed to just draw
                grouped = 0;
            } else if let Ok(state) = self.rx.try_recv() {
                // Only group draw calls if there is another draw already queued
                recv_peek = Some(state);
                grouped += 1;
                continue;
            } else {
                // No more draws queued, proceed to just draw
                grouped = 0;
            }

            let mut lines = vec![];

            // Make orphaned lines appear at the top, so they can be properly
            // forgotten.
            let orphan_lines_count = orphan_lines.len();
            lines.append(&mut orphan_lines);

            for index in state.ordering.iter() {
                let obj = &state.objects[*index];
                if let Some(ref draw_state) = obj.draw_state {
                    lines.extend_from_slice(&draw_state.lines[..]);
                }
            }

            let finished = state.is_done();
            state.draw_target.apply_draw_state(ProgressDrawState {
                lines,
                orphan_lines: orphan_lines_count,
                force_draw: force_draw || orphan_lines_count > 0,
                move_cursor,
                finished,
            })?;

            force_draw = false;
        }

        if clear {
            let mut state = self.state.write().unwrap();
            state.draw_target.apply_draw_state(ProgressDrawState {
                lines: vec![],
                orphan_lines: 0,
                finished: true,
                force_draw: true,
                move_cursor,
            })?;
        }

        self.joining.store(false, Ordering::Release);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_pb_drop() {
        let pb = ProgressBar::new(10);
        let mpb = MultiProgress::new();
        // This clone call is required to trigger a now fixed bug.
        // See <https://github.com/mitsuhiko/indicatif/pull/141> for context
        #[allow(clippy::redundant_clone)]
        mpb.add(pb.clone());
    }

    #[test]
    fn it_can_wrap_a_reader() {
        let bytes = &b"I am an implementation of io::Read"[..];
        let pb = ProgressBar::new(bytes.len() as u64);
        let mut reader = pb.wrap_read(bytes);
        let mut writer = Vec::new();
        io::copy(&mut reader, &mut writer).unwrap();
        assert_eq!(writer, bytes);
    }

    #[test]
    fn it_can_wrap_a_writer() {
        let bytes = b"implementation of io::Read";
        let mut reader = &bytes[..];
        let pb = ProgressBar::new(bytes.len() as u64);
        let writer = Vec::new();
        let mut writer = pb.wrap_write(writer);
        io::copy(&mut reader, &mut writer).unwrap();
        assert_eq!(writer.it, bytes);
    }

    #[test]
    fn progress_bar_sync_send() {
        let _: Box<dyn Sync> = Box::new(ProgressBar::new(1));
        let _: Box<dyn Send> = Box::new(ProgressBar::new(1));
        let _: Box<dyn Sync> = Box::new(MultiProgress::new());
        let _: Box<dyn Send> = Box::new(MultiProgress::new());
    }
}
