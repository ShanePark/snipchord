//! Small, shared X11 helpers used by capture and overlay code.
//!
//! The application intentionally keeps one protocol connection for its lifetime.  This avoids
//! repeatedly opening an X connection for warm captures and makes ownership of the input grabs,
//! selection window and clipboard owner explicit to the event loop.

use std::error::Error;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::image::{Image, PixelLayout};
use x11rb::protocol::xinerama::ConnectionExt as XineramaConnectionExt;
use x11rb::protocol::xproto::{
    Atom, ClientMessageData, ClientMessageEvent, ConnectionExt, CreateGCAux, CreateWindowAux,
    EventMask, Format, Gcontext, Pixmap, PropMode, Screen, VisualClass, Visualid, Visualtype,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::CURRENT_TIME;

pub type X11Result<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// The root screen and its native image format.
pub struct X11Context {
    pub conn: RustConnection,
    pub screen_num: usize,
    pub screen: Screen,
    pub root_layout: PixelLayout,
}

/// Result of trying to claim the per-display SnipChord instance slot.
pub enum InstanceClaim {
    /// This process owns `window` and should enter the resident event loop.
    Owner(Instance),
    /// Another process owns the slot. The caller can send it a command and exit.
    Existing(Window),
}

/// A hidden X11 window that owns the SnipChord singleton selection.
pub struct Instance {
    pub window: Window,
    instance_atom: Atom,
    command_atom: Atom,
}

impl X11Context {
    /// Connect to the display named by `$DISPLAY` and validate the root visual.
    pub fn connect() -> X11Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)?;
        // Large screen pixmaps are split according to the BIG-REQUESTS limit.
        // Ask for that limit while the resident connection is being initialized
        // so the first screenshot does not pay this round trip immediately
        // before its first PutImage upload.
        conn.prefetch_maximum_request_bytes();
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .cloned()
            .ok_or("X11 screen index is out of range")?;
        let root_visual_type = find_visual(&screen, screen.root_visual)
            .ok_or("X11 root visual description is missing")?;
        if root_visual_type.class != VisualClass::TRUE_COLOR {
            return Err("SnipChord requires a TrueColor X11 root visual".into());
        }
        if screen.width_in_pixels == 0 || screen.height_in_pixels == 0 {
            return Err("X11 root screen has no drawable pixels".into());
        }
        let root_layout = PixelLayout::from_visual_type(root_visual_type)?;
        Ok(Self {
            conn,
            screen_num,
            screen,
            root_layout,
        })
    }

    pub fn root(&self) -> Window {
        self.screen.root
    }

    pub fn width(&self) -> u16 {
        self.screen.width_in_pixels
    }

    pub fn height(&self) -> u16 {
        self.screen.height_in_pixels
    }

    pub fn depth(&self) -> u8 {
        self.screen.root_depth
    }

    pub fn visual(&self) -> Visualid {
        self.screen.root_visual
    }

    /// Refresh the root drawable dimensions after a monitor hotplug or a
    /// desktop resize.  The initial setup values are stable for most sessions,
    /// but GetGeometry is cheap and keeps the next capture authoritative.
    pub fn refresh_root_geometry(&mut self) -> X11Result<()> {
        let geometry = self.conn.get_geometry(self.root())?.reply()?;
        if geometry.width == 0 || geometry.height == 0 {
            return Err("X11 root screen has no drawable pixels".into());
        }
        self.screen.width_in_pixels = geometry.width;
        self.screen.height_in_pixels = geometry.height;
        Ok(())
    }

    /// Return the current pointer location in root-window coordinates.
    pub fn pointer_position(&self) -> X11Result<(i32, i32)> {
        let reply = self.conn.query_pointer(self.root())?.reply()?;
        Ok((i32::from(reply.root_x), i32::from(reply.root_y)))
    }

    /// Return active monitor rectangles in root-window coordinates.
    ///
    /// Xinerama is optional even on an X11 desktop.  Callers can fall back to
    /// the root rectangle when the extension is absent, so this helper keeps
    /// that protocol detail out of the UI event loop.
    pub fn monitor_rects(&self) -> X11Result<Vec<(i32, i32, u32, u32)>> {
        let fallback = || vec![(0, 0, u32::from(self.width()), u32::from(self.height()))];
        let reply = match self.conn.xinerama_query_screens() {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => reply,
                Err(_) => return Ok(fallback()),
            },
            Err(_) => return Ok(fallback()),
        };
        let monitors: Vec<_> = reply
            .screen_info
            .into_iter()
            .filter(|monitor| monitor.width != 0 && monitor.height != 0)
            .map(|monitor| {
                (
                    i32::from(monitor.x_org),
                    i32::from(monitor.y_org),
                    u32::from(monitor.width),
                    u32::from(monitor.height),
                )
            })
            .collect();
        if monitors.is_empty() {
            Ok(fallback())
        } else {
            Ok(monitors)
        }
    }

    /// Atomically claim the per-display resident instance slot.
    ///
    /// A short server grab closes the check-then-claim race between two hotkey invocations. The
    /// grab is released on every path before this method returns. The selection is deliberately
    /// separate from `CLIPBOARD`; destroying the hidden owner window releases it automatically.
    pub fn claim_instance(&self) -> X11Result<InstanceClaim> {
        let instance_atom = self.intern_atom(b"_SNIPCHORD_INSTANCE")?;
        let command_atom = self.intern_atom(b"_SNIPCHORD_COMMAND")?;
        self.conn.grab_server()?.check()?;

        let result = (|| {
            let existing = self.conn.get_selection_owner(instance_atom)?.reply()?.owner;
            if existing != 0 {
                return Ok(InstanceClaim::Existing(existing));
            }

            let window = self.create_input_only_window(
                1,
                1,
                &CreateWindowAux::new().event_mask(EventMask::NO_EVENT),
            )?;
            self.conn
                .set_selection_owner(window, instance_atom, CURRENT_TIME)?
                .check()?;
            let owner = self.conn.get_selection_owner(instance_atom)?.reply()?.owner;
            if owner != window {
                self.conn.destroy_window(window)?.check()?;
                return Ok(InstanceClaim::Existing(owner));
            }
            Ok(InstanceClaim::Owner(Instance {
                window,
                instance_atom,
                command_atom,
            }))
        })();

        // Always release the server grab, even if an allocation or request failed.
        let ungrab_result = self.conn.ungrab_server()?.check();
        self.conn.flush()?;
        match (result, ungrab_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }

    /// Return the current resident owner, if any, without creating a window or
    /// claiming the selection.  This is used by `--quit`: an informational
    /// request must never turn an idle invocation into a new daemon.
    pub fn instance_owner(&self) -> X11Result<Option<Window>> {
        let instance_atom = self.intern_atom(b"_SNIPCHORD_INSTANCE")?;
        let owner = self.conn.get_selection_owner(instance_atom)?.reply()?.owner;
        Ok((owner != 0).then_some(owner))
    }

    /// Send a compact command to a resident instance's hidden window.
    pub fn send_instance_command(&self, owner: Window, command: u32) -> X11Result<()> {
        let command_atom = self.intern_atom(b"_SNIPCHORD_COMMAND")?;
        let event = instance_command_event(owner, command_atom, command);
        self.conn
            .send_event(false, owner, EventMask::NO_EVENT, event)?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Extract a command sent to this process's hidden instance window.
    pub fn instance_command(
        &self,
        instance: &Instance,
        event: &x11rb::protocol::Event,
    ) -> Option<u32> {
        match event {
            x11rb::protocol::Event::ClientMessage(message)
                if message.window == instance.window
                    && message.type_ == instance.command_atom
                    && message.format == 32 =>
            {
                Some(message.data.as_data32()[0])
            }
            _ => None,
        }
    }

    /// Return the server's pixmap format for a depth.  This is useful to code that needs to
    /// inspect the raw bytes rather than use `Image`'s pixel accessors.
    pub fn format_for_depth(&self, depth: u8) -> X11Result<Format> {
        self.conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .copied()
            .ok_or_else(|| "X11 pixmap format is missing".into())
    }

    pub fn alloc_id(&self) -> X11Result<u32> {
        Ok(self.conn.generate_id()?)
    }

    pub fn create_pixmap(&self, width: u16, height: u16) -> X11Result<Pixmap> {
        let pixmap = self.alloc_id()?;
        self.conn
            .create_pixmap(self.depth(), pixmap, self.root(), width, height)?;
        Ok(pixmap)
    }

    pub fn create_gc(&self, drawable: u32, aux: &CreateGCAux) -> X11Result<Gcontext> {
        let gc = self.alloc_id()?;
        self.conn.create_gc(gc, drawable, aux)?;
        Ok(gc)
    }

    /// Upload an image into a pixmap, honoring the server's request-size limit and native layout.
    pub fn put_image(&self, image: &Image<'_>, drawable: Pixmap, gc: Gcontext) -> X11Result<()> {
        let cookies = image.put(&self.conn, drawable, gc, 0, 0)?;
        // Queue every split PutImage request before checking any of them. The
        // first `check` inserts one synchronization request for the batch;
        // checking cookies while they are still being queued can make the
        // connection flush repeatedly on high-latency X11 transports.
        self.conn.flush()?;
        for cookie in cookies {
            cookie.check()?;
        }
        Ok(())
    }

    pub fn create_window(
        &self,
        width: u16,
        height: u16,
        aux: &CreateWindowAux,
    ) -> X11Result<Window> {
        let window = self.alloc_id()?;
        self.conn.create_window(
            self.depth(),
            window,
            self.root(),
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual(),
            aux,
        )?;
        Ok(window)
    }

    pub fn create_input_only_window(
        &self,
        width: u16,
        height: u16,
        aux: &CreateWindowAux,
    ) -> X11Result<Window> {
        let window = self.alloc_id()?;
        self.conn.create_window(
            0,
            window,
            self.root(),
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_ONLY,
            0,
            aux,
        )?;
        Ok(window)
    }

    pub fn intern_atom(&self, name: &[u8]) -> X11Result<Atom> {
        Ok(self.conn.intern_atom(false, name)?.reply()?.atom)
    }

    pub fn change_property8(
        &self,
        window: Window,
        property: Atom,
        type_: Atom,
        data: &[u8],
    ) -> X11Result<()> {
        self.conn
            .change_property8(PropMode::REPLACE, window, property, type_, data)?
            .check()?;
        Ok(())
    }

    pub fn flush(&self) -> X11Result<()> {
        self.conn.flush()?;
        Ok(())
    }
}

impl Instance {
    /// Release the instance selection and hidden window before application shutdown.
    pub fn release(self, context: &X11Context) -> X11Result<()> {
        // A selection can be stolen by another client after this instance was
        // created. Never clear a newer owner's selection while shutting down.
        let owner = context
            .conn
            .get_selection_owner(self.instance_atom)?
            .reply()?
            .owner;
        if owner == self.window {
            context
                .conn
                .set_selection_owner(0u32, self.instance_atom, CURRENT_TIME)?
                .check()?;
        }
        context.conn.destroy_window(self.window)?.check()?;
        context.conn.flush()?;
        Ok(())
    }
}

fn instance_command_event(
    destination: Window,
    command_atom: Atom,
    command: u32,
) -> ClientMessageEvent {
    ClientMessageEvent::new(
        32,
        destination,
        command_atom,
        ClientMessageData::from([command, 0, 0, 0, 0]),
    )
}

fn find_visual(screen: &Screen, visual: Visualid) -> Option<Visualtype> {
    screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|candidate| candidate.visual_id == visual)
        .copied()
}

/// Encode an 8-bit RGB tuple into a packed X11 pixel value.
pub fn encode_rgb(layout: PixelLayout, rgb: [u8; 3]) -> u32 {
    layout.encode((
        u16::from(rgb[0]) * 257,
        u16::from(rgb[1]) * 257,
        u16::from(rgb[2]) * 257,
    ))
}
