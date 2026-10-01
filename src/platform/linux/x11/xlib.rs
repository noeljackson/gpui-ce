use anyhow::{Context as _, anyhow, bail};
use std::{cell::OnceCell, ffi::CString, ptr::NonNull, rc::Rc, thread::ThreadId};
use x11_dl::{xlib, xlib_xcb};
use x11rb::{
    connection::Connection,
    protocol::xproto::{self, ConnectionExt as _},
    wrapper::ConnectionExt as _,
    xcb_ffi::XCBConnection,
};

pub(crate) struct XlibDisplayOwner {
    display: OnceCell<Result<OwnedDisplay, String>>,
    name: CString,
    original: Rc<XCBConnection>,
    thread: ThreadId,
}

impl XlibDisplayOwner {
    pub(super) fn new(name: CString, original: Rc<XCBConnection>) -> Self {
        Self {
            name,
            original,
            thread: std::thread::current().id(),
            display: OnceCell::new(),
        }
    }

    pub(super) fn display(
        &self,
        window: u32,
        screen: usize,
        visual: u32,
    ) -> anyhow::Result<NonNull<std::ffi::c_void>> {
        if std::thread::current().id() != self.thread {
            bail!("Xlib native handles must be used on the GPUI foreground thread");
        }
        let display = self.display.get_or_init(|| {
            OwnedDisplay::open(&self.name, &self.original).map_err(|error| format!("{error:#}"))
        });
        let display = display.as_ref().map_err(|error| anyhow!("{error}"))?;
        let root = display
            .connection()?
            .setup()
            .roots
            .get(screen)
            .context("native X11 screen is unavailable")?;
        let geometry = display.connection()?.get_geometry(window)?.reply()?;
        let attributes = display
            .connection()?
            .get_window_attributes(window)?
            .reply()?;
        if geometry.root != root.root || attributes.visual != visual {
            bail!("native Xlib window does not match the retained XCB window");
        }
        Ok(display.display.cast())
    }
}

struct OwnedDisplay {
    functions: xlib::Xlib,
    display: NonNull<xlib::Display>,
    connection: Option<XCBConnection>,
}

impl OwnedDisplay {
    fn open(name: &CString, original: &XCBConnection) -> anyhow::Result<Self> {
        let functions = xlib::Xlib::open().context("load Xlib for native window handles")?;
        let bridge = xlib_xcb::Xlib_xcb::open().context("load Xlib-XCB for display affinity")?;
        // This connection never owns GPUI's event queue. All access is confined to
        // the foreground thread; the renderer continues using its existing XCB connection.
        let display = NonNull::new(unsafe { (functions.XOpenDisplay)(name.as_ptr()) })
            .context("open the retained GPUI X11 display with Xlib")?;
        let mut owned = Self {
            functions,
            display,
            connection: None,
        };
        let connection = unsafe { (bridge.XGetXCBConnection)(display.as_ptr()) };
        if connection.is_null() {
            bail!("Xlib has no associated XCB connection");
        }
        // Requests/replies only: never read events, change event-queue ownership,
        // or disconnect this borrowed view. XCloseDisplay remains the sole owner.
        owned.connection =
            Some(unsafe { XCBConnection::from_raw_xcb_connection(connection, false) }?);
        let roots = &owned.connection()?.setup().roots;
        if roots.len() != original.setup().roots.len()
            || roots
                .iter()
                .zip(&original.setup().roots)
                .any(|(left, right)| left.root != right.root)
        {
            bail!("Xlib and XCB display screens differ");
        }

        // Root IDs can coincide on different servers. A private unmapped window
        // carrying a fresh value binds the connections to the same live server.
        let window = original.generate_id()?;
        let mut probe = AffinityProbe {
            connection: original,
            window: Some(window),
        };
        let proof = (|| -> anyhow::Result<()> {
            let root = original
                .setup()
                .roots
                .first()
                .context("XCB display has no screens")?
                .root;
            original
                .create_window(
                    0,
                    window,
                    root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    xproto::WindowClass::INPUT_ONLY,
                    0,
                    &xproto::CreateWindowAux::default(),
                )?
                .check()?;
            let nonce: [u8; 16] = rand::random();
            original
                .change_property8(
                    xproto::PropMode::REPLACE,
                    window,
                    xproto::AtomEnum::WM_NAME,
                    xproto::AtomEnum::STRING,
                    &nonce,
                )?
                .check()?;
            original.flush()?;
            let property = owned
                .connection()?
                .get_property(
                    false,
                    window,
                    xproto::AtomEnum::WM_NAME,
                    xproto::AtomEnum::STRING,
                    0,
                    4,
                )?
                .reply()?;
            if property.type_ != u32::from(xproto::AtomEnum::STRING)
                || property.format != 8
                || property.bytes_after != 0
                || property.value != nonce
            {
                bail!("Xlib connection is not bound to the retained XCB server");
            }
            Ok(())
        })();
        let cleanup = probe.destroy();
        match (proof, cleanup) {
            (Ok(()), Ok(())) => {}
            (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
            (Err(error), Err(cleanup)) => {
                return Err(anyhow!(
                    "{error:#}; affinity cleanup also failed: {cleanup:#}"
                ));
            }
        }
        Ok(owned)
    }

    fn connection(&self) -> anyhow::Result<&XCBConnection> {
        self.connection
            .as_ref()
            .context("Xlib affinity connection is unavailable")
    }
}

impl Drop for OwnedDisplay {
    fn drop(&mut self) {
        // The non-owning XCB wrapper must not outlive the display it borrows.
        drop(self.connection.take());
        let status = unsafe { (self.functions.XCloseDisplay)(self.display.as_ptr()) };
        if status != 0 {
            log::error!("XCloseDisplay returned {status} for the native handle connection");
        }
    }
}

struct AffinityProbe<'a> {
    connection: &'a XCBConnection,
    window: Option<u32>,
}

impl AffinityProbe<'_> {
    fn destroy(&mut self) -> anyhow::Result<()> {
        if let Some(window) = self.window {
            self.connection.destroy_window(window)?.check()?;
            self.connection.flush()?;
            self.window = None;
        }
        Ok(())
    }
}

impl Drop for AffinityProbe<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.destroy() {
            log::error!("failed to clean up the owned X11 affinity probe: {error:#}");
        }
    }
}
