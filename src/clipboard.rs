//! A small X11 CLIPBOARD selection owner.
//!
//! GTK's `Clipboard::set_image` is convenient, but it also keeps a toolkit
//! runtime alive and hides the selection protocol.  SnipChord owns a tiny
//! input-only window instead.  It advertises the same useful image targets as
//! the original implementation (`image/png` and `image/bmp`) and keeps the
//! owner alive until the resident process exits.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as XProtoConnectionExt, EventMask,
    Property, SelectionNotifyEvent, SelectionRequestEvent, Timestamp, Window,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::ErrorKind;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

use crate::image::RgbaImage;

/// Fallible result used by the X11 clipboard owner.
pub type ClipboardResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const CURRENT_TIME: Timestamp = 0;
const STALE_TRANSFER_AFTER: Duration = Duration::from_secs(60);
// Keep property requests small even when BIG-REQUESTS raises the server's
// nominal request limit.  This bounds event-loop bursts and makes large image
// transfers use the same tested INCR path across X servers.
const MAX_CLIPBOARD_CHUNK: usize = 64 * 1024;

/// Errors specific to clipboard state transitions.
#[derive(Debug)]
pub struct ClipboardError {
    message: String,
}

impl ClipboardError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ClipboardError {}

/// The atoms used by [`Clipboard`].  The fields are public so an application
/// that needs to inspect an X11 selection can use exactly the atoms the owner
/// advertises, while normal callers only need `handle_event`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardAtoms {
    pub clipboard: Atom,
    pub targets: Atom,
    pub timestamp: Atom,
    pub incr: Atom,
    pub image_png: Atom,
    pub image_bmp: Atom,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TransferKey {
    requestor: Window,
    property: Atom,
}

struct Transfer {
    target: Atom,
    data: Arc<Vec<u8>>,
    offset: usize,
    last_activity: Instant,
}

/// An X11 selection owner with lazy PNG/BMP payload encoding and INCR support
/// for images larger than a single X11 request.
pub struct Clipboard {
    owner: Window,
    atoms: ClipboardAtoms,
    image: Option<RgbaImage>,
    png: Option<Arc<Vec<u8>>>,
    bmp: Option<Arc<Vec<u8>>>,
    // `None` means the caller claimed with CurrentTime and did not provide a
    // server timestamp.  TIMESTAMP is omitted from TARGETS in that case.
    timestamp: Option<Timestamp>,
    transfers: HashMap<TransferKey, Transfer>,
    watched_requestors: HashSet<Window>,
}

impl Clipboard {
    /// Intern the selection and image MIME atoms for a persistent owner
    /// window.  The window should outlive this object and must be kept alive
    /// while the process advertises a clipboard image.
    pub fn new<C: Connection>(conn: &C, owner: Window) -> ClipboardResult<Self> {
        let atoms = ClipboardAtoms {
            clipboard: intern_atom(conn, b"CLIPBOARD")?,
            targets: intern_atom(conn, b"TARGETS")?,
            timestamp: intern_atom(conn, b"TIMESTAMP")?,
            incr: intern_atom(conn, b"INCR")?,
            image_png: intern_atom(conn, b"image/png")?,
            image_bmp: intern_atom(conn, b"image/bmp")?,
        };
        Ok(Self {
            owner,
            atoms,
            image: None,
            png: None,
            bmp: None,
            timestamp: None,
            transfers: HashMap::new(),
            watched_requestors: HashSet::new(),
        })
    }

    /// Return the expiry deadline for the next in-progress INCR transfer.
    /// The event loop can use this to schedule a timer while X11 is idle.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.transfers
            .values()
            .map(|transfer| transfer.last_activity + STALE_TRANSFER_AFTER)
            .min()
    }

    /// Expire abandoned INCR transfers.  Event handling calls this too, so a
    /// timer is only needed while a requestor remains idle.
    pub fn tick<C: Connection>(&mut self, conn: &C) -> ClipboardResult<()> {
        self.expire_stale_transfers(conn);
        Ok(())
    }

    /// Claim CLIPBOARD for `owner` and retain an owned copy of the image.
    ///
    /// PNG and BMP bytes are encoded only when a requestor asks for that
    /// target, so an image can be copied without paying both encoding costs on
    /// the capture path.  Pass the timestamp from the input event when one is
    /// available; with `0`, the selection is claimed using CurrentTime and
    /// TIMESTAMP is omitted because the exact server time is unknown.
    pub fn set_image<C: Connection>(
        &mut self,
        conn: &C,
        image: &RgbaImage,
        timestamp: Timestamp,
    ) -> ClipboardResult<()> {
        self.reset_transfers(conn);
        self.image = Some(image.clone());
        self.png = None;
        self.bmp = None;
        self.timestamp = (timestamp != CURRENT_TIME).then_some(timestamp);

        conn.set_selection_owner(self.owner, self.atoms.clipboard, timestamp)?
            .check()?;
        conn.flush()?;
        let current_owner = conn
            .get_selection_owner(self.atoms.clipboard)?
            .reply()?
            .owner;
        if current_owner != self.owner {
            self.image = None;
            self.png = None;
            self.bmp = None;
            self.timestamp = None;
            return Err(Box::new(ClipboardError::new(
                "X11 did not grant the CLIPBOARD selection",
            )));
        }
        Ok(())
    }

    /// Release the selection and discard image data.  Destroying the owner
    /// window also releases it, but an explicit release lets the application
    /// stop cleanly while retaining its X11 connection for diagnostics.
    pub fn release<C: Connection>(&mut self, conn: &C) -> ClipboardResult<()> {
        self.reset_transfers(conn);
        self.image = None;
        self.png = None;
        self.bmp = None;
        self.timestamp = None;
        // A SelectionClear event can race with shutdown.  Do not clear a new
        // owner's clipboard after another client has taken the selection.
        let current_owner = conn
            .get_selection_owner(self.atoms.clipboard)?
            .reply()?
            .owner;
        if current_owner == self.owner {
            conn.set_selection_owner(0u32, self.atoms.clipboard, CURRENT_TIME)?
                .check()?;
        }
        conn.flush()?;
        Ok(())
    }

    /// Handle events delivered to the application's X11 connection.
    ///
    /// The caller should invoke this for every event before dispatching UI
    /// events.  `true` means that the event belonged to this clipboard owner.
    /// A failed transfer to a destroyed requestor is cleaned up and reported
    /// as handled, keeping a stale client from terminating the resident app.
    pub fn handle_event<C: Connection>(
        &mut self,
        conn: &C,
        event: &Event,
    ) -> ClipboardResult<bool> {
        self.tick(conn)?;
        match event {
            Event::SelectionRequest(request)
                if request.owner == self.owner && request.selection == self.atoms.clipboard =>
            {
                match self.serve_request(conn, request) {
                    Ok(()) => Ok(true),
                    Err(error) if is_dead_requestor_error(error.as_ref()) => {
                        // BadWindow can arrive after a requestor exits between
                        // SelectionRequest and the property/notify requests.
                        // Treat that client as gone and keep the resident app
                        // alive.
                        self.drop_requestor(conn, request.requestor);
                        Ok(true)
                    }
                    Err(error) => Err(error),
                }
            }
            Event::SelectionClear(clear)
                if clear.owner == self.owner && clear.selection == self.atoms.clipboard =>
            {
                self.image = None;
                self.png = None;
                self.bmp = None;
                self.timestamp = None;
                self.reset_transfers(conn);
                Ok(true)
            }
            Event::PropertyNotify(property)
                if property.state == Property::DELETE
                    && self.transfers.contains_key(&TransferKey {
                        requestor: property.window,
                        property: property.atom,
                    }) =>
            {
                self.send_next_chunk(conn, property.window, property.atom);
                Ok(true)
            }
            Event::DestroyNotify(destroy)
                if self
                    .transfers
                    .keys()
                    .any(|key| key.requestor == destroy.window) =>
            {
                self.drop_requestor(conn, destroy.window);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn serve_request<C: Connection>(
        &mut self,
        conn: &C,
        request: &SelectionRequestEvent,
    ) -> ClipboardResult<()> {
        // A request with property == NONE uses the target atom as its property
        // by ICCCM convention.
        let property = if request.property == u32::from(AtomEnum::NONE) {
            request.target
        } else {
            request.property
        };

        if self.image.is_none() {
            return self.send_selection_notify(conn, request, u32::from(AtomEnum::NONE));
        }

        if request.target == self.atoms.targets {
            let mut targets = vec![self.atoms.targets];
            if self.timestamp.is_some() {
                targets.push(self.atoms.timestamp);
            }
            targets.extend([self.atoms.image_png, self.atoms.image_bmp]);
            conn.change_property32(
                x11rb::protocol::xproto::PropMode::REPLACE,
                request.requestor,
                property,
                AtomEnum::ATOM,
                &targets.to_vec(),
            )?
            .check()?;
            return self.send_selection_notify(conn, request, property);
        }

        if request.target == self.atoms.timestamp {
            let Some(timestamp) = self.timestamp else {
                return self.send_selection_notify(conn, request, u32::from(AtomEnum::NONE));
            };
            conn.change_property32(
                x11rb::protocol::xproto::PropMode::REPLACE,
                request.requestor,
                property,
                AtomEnum::INTEGER,
                &[timestamp],
            )?
            .check()?;
            return self.send_selection_notify(conn, request, property);
        }

        if request.target != self.atoms.image_png && request.target != self.atoms.image_bmp {
            return self.send_selection_notify(conn, request, u32::from(AtomEnum::NONE));
        }

        let key = TransferKey {
            requestor: request.requestor,
            property,
        };
        if self.transfers.contains_key(&key) {
            // Never overwrite an active stream that is waiting for a
            // PropertyNotify delete from this requestor/property pair.
            return self.send_selection_notify(conn, request, u32::from(AtomEnum::NONE));
        }
        let data = self.payload(request.target)?;
        let inline_limit = conn
            .maximum_request_bytes()
            .saturating_sub(64)
            .clamp(1, MAX_CLIPBOARD_CHUNK);
        if data.len() <= inline_limit {
            conn.change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                request.requestor,
                property,
                request.target,
                &data,
            )?
            .check()?;
            return self.send_selection_notify(conn, request, property);
        }

        let length = u32::try_from(data.len()).map_err(|_| {
            Box::new(ClipboardError::new(
                "clipboard payload exceeds X11's 32-bit length",
            )) as Box<dyn Error + Send + Sync>
        })?;
        conn.change_property32(
            x11rb::protocol::xproto::PropMode::REPLACE,
            request.requestor,
            property,
            self.atoms.incr,
            &[length],
        )?
        .check()?;
        self.transfers.insert(
            key,
            Transfer {
                target: request.target,
                data,
                offset: 0,
                last_activity: Instant::now(),
            },
        );
        if let Err(error) = self.watch_requestor(conn, request.requestor) {
            self.transfers.remove(&TransferKey {
                requestor: request.requestor,
                property,
            });
            return Err(error);
        }
        self.send_selection_notify(conn, request, property)
    }

    fn payload(&mut self, target: Atom) -> ClipboardResult<Arc<Vec<u8>>> {
        if target == self.atoms.image_png {
            if self.png.is_none() {
                let image = self.image.as_ref().ok_or_else(|| {
                    Box::new(ClipboardError::new("no image is currently claimed"))
                        as Box<dyn Error + Send + Sync>
                })?;
                self.png = Some(Arc::new(image.to_png()?));
            }
            return Ok(self
                .png
                .as_ref()
                .expect("PNG cache was just initialized")
                .clone());
        }
        if target == self.atoms.image_bmp {
            if self.bmp.is_none() {
                let image = self.image.as_ref().ok_or_else(|| {
                    Box::new(ClipboardError::new("no image is currently claimed"))
                        as Box<dyn Error + Send + Sync>
                })?;
                self.bmp = Some(Arc::new(image.to_bmp()?));
            }
            return Ok(self
                .bmp
                .as_ref()
                .expect("BMP cache was just initialized")
                .clone());
        }
        Err(Box::new(ClipboardError::new(
            "unsupported clipboard target",
        )))
    }

    fn send_selection_notify<C: Connection>(
        &self,
        conn: &C,
        request: &SelectionRequestEvent,
        property: Atom,
    ) -> ClipboardResult<()> {
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify)?
            .check()?;
        conn.flush()?;
        Ok(())
    }

    /// Send the next INCR chunk after the requestor deletes its property.
    /// Errors are deliberately swallowed after the transfer is removed: a
    /// requestor can disappear between its DeleteProperty and our response.
    fn send_next_chunk<C: Connection>(&mut self, conn: &C, requestor: Window, property: Atom) {
        let key = TransferKey {
            requestor,
            property,
        };
        let Some((target, data, offset)) = self
            .transfers
            .get(&key)
            .map(|transfer| (transfer.target, transfer.data.clone(), transfer.offset))
        else {
            return;
        };

        let chunk_size = conn
            .maximum_request_bytes()
            .saturating_sub(64)
            .clamp(1, MAX_CLIPBOARD_CHUNK);
        let result = if offset < data.len() {
            let end = offset.saturating_add(chunk_size).min(data.len());
            let result: ClipboardResult<()> = match conn.change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                requestor,
                property,
                target,
                &data[offset..end],
            ) {
                Ok(cookie) => cookie
                    .check()
                    .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>),
                Err(error) => Err(Box::new(error) as Box<dyn Error + Send + Sync>),
            };
            if result.is_ok() {
                if let Some(transfer) = self.transfers.get_mut(&key) {
                    transfer.offset = end;
                    transfer.last_activity = Instant::now();
                }
            }
            result
        } else {
            // A zero-length final property terminates an INCR transfer.
            match conn.change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                requestor,
                property,
                target,
                &[],
            ) {
                Ok(cookie) => cookie
                    .check()
                    .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>),
                Err(error) => Err(Box::new(error) as Box<dyn Error + Send + Sync>),
            }
        };

        if result.is_err() || offset >= data.len() {
            self.transfers.remove(&key);
            self.unwatch_if_unused(conn, requestor);
        }
        let _ = conn.flush();
    }

    fn watch_requestor<C: Connection>(
        &mut self,
        conn: &C,
        requestor: Window,
    ) -> ClipboardResult<()> {
        if self.watched_requestors.contains(&requestor) {
            return Ok(());
        }
        conn.change_window_attributes(
            requestor,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
        self.watched_requestors.insert(requestor);
        Ok(())
    }

    fn unwatch_if_unused<C: Connection>(&mut self, conn: &C, requestor: Window) {
        if self.transfers.keys().any(|key| key.requestor == requestor) {
            return;
        }
        if self.watched_requestors.remove(&requestor) {
            if let Ok(cookie) = conn.change_window_attributes(
                requestor,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            ) {
                let _ = cookie.check();
            }
        }
    }

    fn drop_requestor<C: Connection>(&mut self, conn: &C, requestor: Window) {
        self.transfers.retain(|key, _| key.requestor != requestor);
        self.unwatch_if_unused(conn, requestor);
    }

    fn reset_transfers<C: Connection>(&mut self, conn: &C) {
        let requestors: Vec<_> = self.watched_requestors.iter().copied().collect();
        self.transfers.clear();
        for requestor in requestors {
            self.unwatch_if_unused(conn, requestor);
        }
    }

    fn expire_stale_transfers<C: Connection>(&mut self, conn: &C) {
        let now = Instant::now();
        let stale_requestors: Vec<_> = self
            .transfers
            .iter()
            .filter(|(_, transfer)| {
                now.duration_since(transfer.last_activity) > STALE_TRANSFER_AFTER
            })
            .map(|(key, _)| key.requestor)
            .collect();
        for requestor in stale_requestors {
            self.drop_requestor(conn, requestor);
        }
    }
}

fn intern_atom<C: Connection>(conn: &C, name: &[u8]) -> ClipboardResult<Atom> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn is_dead_requestor_error(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    matches!(
        error.downcast_ref::<ReplyError>(),
        Some(ReplyError::X11Error(x11_error))
            if x11_error.error_kind == ErrorKind::Window
    )
}
