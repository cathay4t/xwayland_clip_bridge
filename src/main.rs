// SPDX-License-Identifier: Apache 2.0
//
// xwayland-clip-bridge - keep the X11 CLIPBOARD selection bridge-friendly.
//
// When a native Wayland application (Firefox, LibreOffice, ...) owns the
// clipboard, Xwayland mirrors the Wayland offer into the X11 selection and
// advertises X11-only flavor atoms (SAVE_TARGETS, COMPOUND_TEXT, text/_moz_*)
// as X11 targets.  For some of those targets the Wayland source never
// delivers data and Xwayland never sends a failure SelectionNotify either.
// X11 clients that enumerate every advertised target then block until their
// own timeout (Qt's QXcbClipboard: 5000 ms).  Chromium/CEF applications
// running under Xwayland (e.g. DingTalk, which embeds Qt) freeze ~5 s per
// such target before a paste completes.
//
// This daemon watches the X11 CLIPBOARD selection.  When the clipboard
// carries rich content (text/html), it reads the useful formats from the real
// owner with short timeouts, then takes ownership of the selection itself and
// serves a clean target list that it can always answer immediately:
//
//   TARGETS, TIMESTAMP, UTF8_STRING, text/plain;charset=utf-8,
//   text/plain, TEXT, STRING and text/html (when available)
//
// It never advertises SAVE_TARGETS/COMPOUND_TEXT/moz flavors, and every
// request is answered at once - with data or with an explicit refusal
// (property=None).  Wayland clients keep working: Xwayland mirrors the
// daemon's selection back to Wayland.

use std::{
    env,
    error::Error,
    process::exit,
    thread::sleep,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        Event,
        xfixes::{
            ConnectionExt as _, SelectionEventMask,
            SelectionNotifyEvent as XfixesSelectionNotify,
        },
        xproto::{
            Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask,
            PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
            SelectionRequestEvent, Window, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

const READ_TIMEOUT: Duration = Duration::from_millis(1000);
const GRAB_DELAY: Duration = Duration::from_millis(150);
const GRAB_RATE_LIMIT: Duration = Duration::from_millis(400);
const POLL_INTERVAL: Duration = Duration::from_millis(2000);
const GRAB_RETRY_DELAY: Duration = Duration::from_millis(100);
const INCR_CHUNK_BYTES: usize = 64 * 1024;
const INCR_THRESHOLD_BYTES: usize = 128 * 1024;
const INCR_SEND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CLIP_BYTES: usize = 64 * 1024 * 1024;
const MAX_INCR_SENDS: usize = 8;

fn log(msg: &str) {
    eprintln!("xwayland-clip-bridge: {msg}");
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    multiple: Atom,
    utf8: Atom,
    string: Atom,
    text: Atom,
    textplain: Atom,
    textplain_utf8: Atom,
    html: Atom,
    src_prop: Atom,
}

#[derive(Default)]
struct Item {
    data: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RdState {
    Idle,
    Targets,
    Data,
    Incr,
}

struct IncrSend {
    requestor: Window,
    property: Atom,
    target: Atom,
    offset: usize,
    deadline: Instant,
    active: bool,
}

struct Bridge {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    verbose: bool,
    html: Item,
    plain: Item,
    clip_ts: u32,
    have_clip: bool,
    rd: RdState,
    rd_deadline: Instant,
    rd_target: Atom,
    want: Vec<Atom>,
    want_i: usize,
    rd_buf: Vec<u8>,
    new_html: Option<Vec<u8>>,
    new_plain: Option<Vec<u8>>,
    got_html: bool,
    got_plain: bool,
    grab_at: Option<Instant>,
    last_grab: Option<Instant>,
    incr_sends: Vec<IncrSend>,
    last_poll: Instant,
}

impl Bridge {
    fn new(
        conn: RustConnection,
        win: Window,
        atoms: Atoms,
        verbose: bool,
    ) -> Self {
        let mut incr_sends = Vec::with_capacity(MAX_INCR_SENDS);
        for _ in 0..MAX_INCR_SENDS {
            incr_sends.push(IncrSend {
                requestor: 0,
                property: 0,
                target: 0,
                offset: 0,
                deadline: Instant::now(),
                active: false,
            });
        }
        Bridge {
            conn,
            win,
            atoms,
            verbose,
            html: Item::default(),
            plain: Item::default(),
            clip_ts: 0,
            have_clip: false,
            rd: RdState::Idle,
            rd_deadline: Instant::now(),
            rd_target: 0,
            want: Vec::new(),
            want_i: 0,
            rd_buf: Vec::new(),
            new_html: None,
            new_plain: None,
            got_html: false,
            got_plain: false,
            grab_at: None,
            last_grab: None,
            incr_sends,
            last_poll: Instant::now(),
        }
    }

    fn vlog(&self, msg: &str) {
        if self.verbose {
            eprintln!("{msg}");
        }
    }

    fn schedule_grab(&mut self, delay: Duration) {
        let at = Instant::now() + delay;
        if self.grab_at.is_none_or(|cur| at < cur) {
            self.grab_at = Some(at);
        }
    }

    fn reset_read(&mut self) {
        self.rd = RdState::Idle;
        self.rd_buf.clear();
        self.new_html = None;
        self.new_plain = None;
        self.got_html = false;
        self.got_plain = false;
        self.want.clear();
        self.want_i = 0;
    }

    fn start_grab(&mut self) -> Result<(), Box<dyn Error>> {
        let owner = self
            .conn
            .get_selection_owner(self.atoms.clipboard)?
            .reply()?
            .owner;
        if owner == NONE || owner == self.win {
            return Ok(());
        }
        if self.rd != RdState::Idle {
            self.grab_at = Some(Instant::now() + GRAB_RETRY_DELAY);
            return Ok(());
        }
        let now = Instant::now();
        if let Some(last) = self.last_grab {
            if now.duration_since(last) < GRAB_RATE_LIMIT {
                self.grab_at = Some(last + GRAB_RATE_LIMIT);
                return Ok(());
            }
        }
        self.last_grab = Some(now);

        self.new_html = None;
        self.new_plain = None;
        self.got_html = false;
        self.got_plain = false;

        self.vlog(&format!("reading clipboard from owner 0x{owner:x}"));
        self.rd = RdState::Targets;
        self.rd_target = self.atoms.targets;
        self.rd_deadline = now + READ_TIMEOUT;
        self.conn.convert_selection(
            self.win,
            self.atoms.clipboard,
            self.atoms.targets,
            self.atoms.src_prop,
            CURRENT_TIME,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn next_request(&mut self) -> Result<(), Box<dyn Error>> {
        while self.want_i < self.want.len() {
            let target = self.want[self.want_i];
            self.want_i += 1;
            if target == self.atoms.html {
                if self.got_html {
                    continue;
                }
            } else if self.got_plain {
                continue;
            }
            self.rd = RdState::Data;
            self.rd_target = target;
            self.rd_deadline = Instant::now() + READ_TIMEOUT;
            self.conn.convert_selection(
                self.win,
                self.atoms.clipboard,
                target,
                self.atoms.src_prop,
                CURRENT_TIME,
            )?;
            self.conn.flush()?;
            return Ok(());
        }
        self.finish_grab()
    }

    fn finish_grab(&mut self) -> Result<(), Box<dyn Error>> {
        let html = self.new_html.take();
        let plain = self.new_plain.take();
        let have_html =
            self.got_html && html.as_ref().is_some_and(|d| !d.is_empty());

        if have_html {
            for send in self.incr_sends.iter_mut() {
                send.active = false;
            }
            self.html.data = html.unwrap();
            self.plain.data = if self.got_plain {
                plain.unwrap_or_default()
            } else {
                Vec::new()
            };
            self.clip_ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0);
            self.have_clip = true;
            self.conn.set_selection_owner(
                self.win,
                self.atoms.clipboard,
                CURRENT_TIME,
            )?;
            self.conn.flush()?;
            log(&format!(
                "captured clipboard: html {} bytes, text {} bytes; now serving",
                self.html.data.len(),
                self.plain.data.len()
            ));
        } else {
            self.vlog("no rich content captured, not taking ownership");
        }
        self.reset_read();
        Ok(())
    }

    fn fetch_property(
        &self,
        w: Window,
        prop: Atom,
    ) -> Result<(Vec<u8>, Atom, u8), Box<dyn Error>> {
        let mut value = Vec::new();
        let mut offset: u32 = 0;
        let mut type_ = NONE;
        let mut format = 0u8;

        loop {
            let reply = self
                .conn
                .get_property(
                    false,
                    w,
                    prop,
                    AtomEnum::ANY,
                    offset,
                    u32::MAX / 4,
                )?
                .reply()?;
            if format == 0 && !reply.value.is_empty() {
                format = reply.format;
                type_ = reply.type_;
            }
            value.extend_from_slice(&reply.value);
            if reply.bytes_after == 0 {
                break;
            }
            offset += (reply.value.len() as u32).div_ceil(4);
        }
        self.conn.delete_property(w, prop)?;
        Ok((value, type_, format))
    }

    fn handle_selection_notify(
        &mut self,
        ev: &SelectionNotifyEvent,
    ) -> Result<(), Box<dyn Error>> {
        if ev.selection != self.atoms.clipboard || ev.requestor != self.win {
            return Ok(());
        }

        if self.rd == RdState::Targets && ev.target == self.atoms.targets {
            if ev.property == NONE {
                self.vlog("TARGETS request refused by owner");
                self.reset_read();
                return Ok(());
            }
            let (value, _type, format) =
                self.fetch_property(self.win, ev.property)?;
            if format != 32 {
                self.reset_read();
                return Ok(());
            }

            let (mut has_html, mut has_utf8, mut has_tpu) =
                (false, false, false);
            let (mut has_tp, mut has_text, mut has_string) =
                (false, false, false);
            for chunk in value.chunks_exact(4) {
                let atom = u32::from_le_bytes(chunk.try_into().unwrap());
                if atom == self.atoms.html {
                    has_html = true;
                } else if atom == self.atoms.utf8 {
                    has_utf8 = true;
                } else if atom == self.atoms.textplain_utf8 {
                    has_tpu = true;
                } else if atom == self.atoms.textplain {
                    has_tp = true;
                } else if atom == self.atoms.text {
                    has_text = true;
                } else if atom == self.atoms.string {
                    has_string = true;
                }
            }

            self.want.clear();
            if has_html {
                self.want.push(self.atoms.html);
            }
            if has_utf8 {
                self.want.push(self.atoms.utf8);
            }
            if has_tpu {
                self.want.push(self.atoms.textplain_utf8);
            }
            if has_tp {
                self.want.push(self.atoms.textplain);
            }
            if has_text {
                self.want.push(self.atoms.text);
            }
            if has_string {
                self.want.push(self.atoms.string);
            }
            self.want_i = 0;
            if self.want.is_empty() {
                self.vlog("owner advertises no usable text targets");
                self.reset_read();
                return Ok(());
            }
            return self.next_request();
        }

        if self.rd == RdState::Data && ev.target == self.rd_target {
            if ev.property == NONE {
                self.vlog(&format!("target 0x{:x} refused", ev.target));
                return self.next_request();
            }
            let (value, type_, _format) =
                self.fetch_property(self.win, ev.property)?;
            if type_ == self.atoms.incr {
                self.rd = RdState::Incr;
                self.rd_buf.clear();
                self.rd_deadline = Instant::now() + READ_TIMEOUT;
                return Ok(());
            }
            if value.len() > MAX_CLIP_BYTES {
                self.reset_read();
                return Ok(());
            }
            if self.rd_target == self.atoms.html {
                self.got_html = !value.is_empty();
                self.new_html = (!value.is_empty()).then_some(value);
            } else {
                self.got_plain = !value.is_empty();
                self.new_plain = (!value.is_empty()).then_some(value);
            }
            return self.next_request();
        }
        Ok(())
    }

    fn on_incr_property(&mut self) -> Result<(), Box<dyn Error>> {
        let (value, _type, _format) =
            self.fetch_property(self.win, self.atoms.src_prop)?;
        if value.is_empty() {
            let buf = std::mem::take(&mut self.rd_buf);
            let got = !buf.is_empty();
            if self.rd_target == self.atoms.html {
                self.got_html = got;
                self.new_html = got.then_some(buf);
            } else {
                self.got_plain = got;
                self.new_plain = got.then_some(buf);
            }
            return self.next_request();
        }
        if self.rd_buf.len() + value.len() > MAX_CLIP_BYTES {
            self.reset_read();
            return Ok(());
        }
        self.rd_buf.extend_from_slice(&value);
        self.rd_deadline = Instant::now() + READ_TIMEOUT;
        Ok(())
    }

    fn send_selection_notify(
        &self,
        req: &SelectionRequestEvent,
        property: Atom,
    ) -> Result<(), Box<dyn Error>> {
        let ev = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property,
        };
        self.conn
            .send_event(false, req.requestor, EventMask::NO_EVENT, ev)?;
        Ok(())
    }

    fn start_incr_send(
        &mut self,
        requestor: Window,
        property: Atom,
        target: Atom,
    ) {
        for send in self.incr_sends.iter_mut() {
            if !send.active {
                send.requestor = requestor;
                send.property = property;
                send.target = target;
                send.offset = 0;
                send.deadline = Instant::now() + INCR_SEND_TIMEOUT;
                send.active = true;
                return;
            }
        }
        self.vlog("warning: no free INCR send slots");
    }

    fn handle_selection_request(
        &mut self,
        req: &SelectionRequestEvent,
    ) -> Result<(), Box<dyn Error>> {
        let prop = if req.property != NONE {
            req.property
        } else {
            req.target
        };

        if req.selection != self.atoms.clipboard {
            return self.send_selection_notify(req, NONE);
        }

        if req.target == self.atoms.targets {
            let mut list = vec![self.atoms.targets, self.atoms.timestamp];
            if self.have_clip && !self.html.data.is_empty() {
                list.push(self.atoms.html);
            }
            if self.have_clip && !self.plain.data.is_empty() {
                list.extend_from_slice(&[
                    self.atoms.utf8,
                    self.atoms.textplain_utf8,
                    self.atoms.textplain,
                    self.atoms.text,
                    self.atoms.string,
                ]);
            }
            self.conn.change_property32(
                PropMode::REPLACE,
                req.requestor,
                prop,
                AtomEnum::ATOM,
                &list,
            )?;
            return self.send_selection_notify(req, prop);
        }

        if req.target == self.atoms.timestamp {
            let ts = self.clip_ts;
            self.conn.change_property32(
                PropMode::REPLACE,
                req.requestor,
                prop,
                self.atoms.timestamp,
                &[ts],
            )?;
            return self.send_selection_notify(req, prop);
        }

        if req.target == self.atoms.multiple {
            // Not supported; refuse immediately (never leave it hanging).
            return self.send_selection_notify(req, NONE);
        }

        let is_html = req.target == self.atoms.html;
        let is_plain = req.target == self.atoms.utf8
            || req.target == self.atoms.textplain_utf8
            || req.target == self.atoms.textplain
            || req.target == self.atoms.text
            || req.target == self.atoms.string;

        if !is_html && !is_plain {
            // Everything unknown is refused at once - this is the actual fix.
            self.vlog(&format!("refusing target 0x{:x}", req.target));
            return self.send_selection_notify(req, NONE);
        }

        let len = if is_html {
            self.html.data.len()
        } else {
            self.plain.data.len()
        };
        if !self.have_clip || len == 0 {
            return self.send_selection_notify(req, NONE);
        }

        if len > INCR_THRESHOLD_BYTES {
            self.conn.change_property32(
                PropMode::REPLACE,
                req.requestor,
                prop,
                self.atoms.incr,
                &[len as u32],
            )?;
            self.start_incr_send(req.requestor, prop, req.target);
        } else {
            let data = if is_html {
                self.html.data.as_slice()
            } else {
                self.plain.data.as_slice()
            };
            self.conn.change_property8(
                PropMode::REPLACE,
                req.requestor,
                prop,
                req.target,
                data,
            )?;
        }
        self.send_selection_notify(req, prop)
    }

    fn on_property_notify(
        &mut self,
        ev: &x11rb::protocol::xproto::PropertyNotifyEvent,
    ) -> Result<(), Box<dyn Error>> {
        if ev.window != self.win {
            return Ok(());
        }
        if self.rd == RdState::Incr
            && ev.atom == self.atoms.src_prop
            && ev.state == Property::NEW_VALUE
        {
            return self.on_incr_property();
        }
        if ev.state != Property::DELETE {
            return Ok(());
        }

        let Bridge {
            conn,
            html,
            plain,
            incr_sends,
            atoms,
            ..
        } = self;
        for send in incr_sends.iter_mut() {
            if !send.active
                || send.requestor != ev.window
                || send.property != ev.atom
            {
                continue;
            }
            let data: &[u8] = if send.target == atoms.html {
                &html.data
            } else {
                &plain.data
            };
            if send.offset > data.len() {
                send.active = false;
                return Ok(());
            }
            let remaining = data.len() - send.offset;
            let n = remaining.min(INCR_CHUNK_BYTES);
            conn.change_property8(
                PropMode::REPLACE,
                send.requestor,
                send.property,
                send.target,
                &data[send.offset..send.offset + n],
            )?;
            send.offset += n;
            send.deadline = Instant::now() + INCR_SEND_TIMEOUT;
            if n == 0 {
                send.active = false;
            }
            conn.flush()?;
            return Ok(());
        }
        Ok(())
    }

    fn on_xfixes_selection(&mut self, ev: &XfixesSelectionNotify) {
        if ev.selection != self.atoms.clipboard {
            return;
        }
        if ev.owner == self.win {
            return;
        }
        if ev.owner == NONE {
            self.vlog("clipboard cleared by another client");
            return;
        }
        if self.rd != RdState::Idle {
            self.reset_read();
        }
        self.schedule_grab(GRAB_DELAY);
    }

    fn check_timers(&mut self) -> Result<(), Box<dyn Error>> {
        let now = Instant::now();

        if let Some(at) = self.grab_at {
            if now >= at {
                self.grab_at = None;
                self.start_grab()?;
            }
        }

        if self.rd != RdState::Idle && now >= self.rd_deadline {
            match self.rd {
                RdState::Targets => {
                    self.vlog("TARGETS read timed out");
                    self.reset_read();
                }
                RdState::Data => {
                    self.vlog("data read timed out, trying next format");
                    self.next_request()?;
                }
                _ => {
                    self.vlog("INCR read timed out");
                    self.reset_read();
                }
            }
        }

        let now = Instant::now();
        for send in self.incr_sends.iter_mut() {
            if send.active && now >= send.deadline {
                send.active = false;
            }
        }
        Ok(())
    }

    fn poll_owner(&mut self) -> Result<(), Box<dyn Error>> {
        let owner = self
            .conn
            .get_selection_owner(self.atoms.clipboard)?
            .reply()?
            .owner;
        if owner != NONE && owner != self.win && self.rd == RdState::Idle {
            self.schedule_grab(GRAB_DELAY);
        }
        Ok(())
    }

    fn dispatch(&mut self, ev: Event) -> Result<(), Box<dyn Error>> {
        match ev {
            Event::SelectionNotify(e) => self.handle_selection_notify(&e)?,
            Event::SelectionRequest(e) => {
                self.handle_selection_request(&e)?;
                self.conn.flush()?;
            }
            Event::SelectionClear(e) => {
                if e.selection == self.atoms.clipboard {
                    self.vlog("lost clipboard ownership");
                    self.schedule_grab(GRAB_DELAY);
                }
            }
            Event::PropertyNotify(e) => self.on_property_notify(&e)?,
            Event::XfixesSelectionNotify(e) => self.on_xfixes_selection(&e),
            _ => {}
        }
        Ok(())
    }
}

fn intern(conn: &RustConnection, name: &[u8]) -> Result<Atom, Box<dyn Error>> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn usage(name: &str) {
    eprintln!(
        "Usage: {name} [-v] [-h]\n\nBridge the X11 CLIPBOARD selection so \
         that clients which read every\nadvertised target (Chromium/CEF/Qt \
         apps under Xwayland) no longer stall\non X11-only flavors advertised \
         through the Wayland clipboard bridge\n(SAVE_TARGETS, COMPOUND_TEXT, \
         ...)."
    );
}

fn run(verbose: bool) -> Result<(), Box<dyn Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let win = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        win,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"xwayland-clip-bridge",
    )?;

    let atoms = Atoms {
        clipboard: intern(&conn, b"CLIPBOARD")?,
        targets: intern(&conn, b"TARGETS")?,
        timestamp: intern(&conn, b"TIMESTAMP")?,
        incr: intern(&conn, b"INCR")?,
        multiple: intern(&conn, b"MULTIPLE")?,
        utf8: intern(&conn, b"UTF8_STRING")?,
        string: intern(&conn, b"STRING")?,
        text: intern(&conn, b"TEXT")?,
        textplain: intern(&conn, b"text/plain")?,
        textplain_utf8: intern(&conn, b"text/plain;charset=utf-8")?,
        html: intern(&conn, b"text/html")?,
        src_prop: intern(&conn, b"XWCB_SRC")?,
    };

    let xfixes = match conn.xfixes_query_version(5, 0) {
        Ok(cookie) => cookie.reply().is_ok(),
        Err(_) => false,
    };
    if xfixes {
        conn.xfixes_select_selection_input(
            root,
            atoms.clipboard,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )?;
    } else {
        log("XFixes not available, falling back to polling");
    }

    log("started");

    let mut bridge = Bridge::new(conn, win, atoms, verbose);
    bridge.poll_owner()?;
    bridge.conn.flush()?;

    loop {
        bridge.check_timers()?;

        if bridge.last_poll.elapsed() >= POLL_INTERVAL {
            bridge.last_poll = Instant::now();
            bridge.poll_owner()?;
        }

        while let Some(ev) = bridge.conn.poll_for_event()? {
            bridge.dispatch(ev)?;
        }
        bridge.conn.flush()?;
        sleep(Duration::from_millis(20));
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut verbose = false;

    for arg in &args[1..] {
        match arg.as_str() {
            "-v" => verbose = true,
            "-h" | "--help" => {
                usage(&args[0]);
                return;
            }
            other => {
                eprintln!("unknown option: {other}");
                usage(&args[0]);
                exit(2);
            }
        }
    }

    if let Err(e) = run(verbose) {
        log(&format!("fatal: {e}"));
        exit(1);
    }
}
