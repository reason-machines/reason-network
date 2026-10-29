use super::model::{self, Command, Snapshot};
use crate::{
    client::Link,
    proto::{LinkInfo, Request},
};
use anyhow::{Result, bail};
use std::{
    mem::size_of,
    ptr::{null, null_mut},
    time::Duration,
};
use tokio::sync::mpsc;
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            DataExchange::*,
            LibraryLoader::GetModuleHandleW,
            Memory::*,
            SystemServices::{SS_LEFT, SS_NOPREFIX},
        },
        UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::w,
};

const ACTION: u32 = WM_APP + 1;
const LAYOUT: u32 = WM_APP + 2;
const SEARCH: usize = 101;
const WORKING: usize = 102;
const UNREAD: usize = 103;
const REFRESH: usize = 104;
const COPY_ADDRESS: usize = 105;
const COPY_LINK: usize = 106;

enum Update {
    Directory(Box<Snapshot>),
    Copy(String, &'static str),
    Notice(&'static str),
}

// No Rust UI state is borrowed by callbacks. Forward control events to the owning
// message loop, so synchronous native callbacks cannot alias a mutable borrow.
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_COMMAND => {
                PostMessageW(hwnd, ACTION, wp, lp);
                0
            }
            WM_SIZE => {
                PostMessageW(hwnd, LAYOUT, 0, 0);
                0
            }
            WM_DPICHANGED => {
                let rect = &*(lp as *const RECT);
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                PostMessageW(hwnd, LAYOUT, 0, 0);
                0
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lp as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                info.ptMinTrackSize = POINT { x: 900 * dpi / 96, y: 580 * dpi / 96 };
                0
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, message, wp, lp),
        }
    }
}

pub fn run() -> Result<()> {
    let mut ui = Ui::new()?;
    let (commands, receive) = mpsc::channel(8);
    let (updates, mut incoming) = mpsc::channel(8);
    let task = tokio::spawn(poll(receive, updates));
    let result = (|| unsafe {
        if SetTimer(ui.window, 1, 100, None) == 0 {
            bail!("Couldn't start the desktop refresh timer");
        }
        ShowWindow(ui.window, SW_SHOW);
        let mut message = MSG::default();
        loop {
            match GetMessageW(&mut message, null_mut(), 0, 0) {
                -1 => bail!("Couldn't read desktop window messages"),
                0 => break,
                _ => (),
            }
            match message.message {
                WM_TIMER if message.hwnd == ui.window => {
                    while let Ok(update) = incoming.try_recv() {
                        ui.update(update);
                    }
                }
                LAYOUT => ui.layout(),
                ACTION => ui.action(message.wParam, &commands),
                WM_KEYDOWN if message.wParam == b'F' as usize && GetKeyState(VK_CONTROL as i32) < 0 => {
                    SetFocus(ui.search);
                }
                _ => {
                    if IsDialogMessageW(ui.window, &message) == 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            }
        }
        Ok(())
    })();
    task.abort();
    result
}

struct Ui {
    window: HWND,
    font: HFONT,
    dpi: u32,
    title: HWND,
    status: HWND,
    search_label: HWND,
    search: HWND,
    working: HWND,
    unread: HWND,
    refresh: HWND,
    copy_address: HWND,
    copy_link: HWND,
    session_label: HWND,
    sessions: HWND,
    machine_label: HWND,
    machines: HWND,
    notice: HWND,
    directory: Option<crate::proto::Directory>,
    addresses: Vec<String>,
    session_rows: Vec<Vec<String>>,
    machine_rows: Vec<Vec<String>>,
}

impl Drop for Ui {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(self.window) != 0 {
                DestroyWindow(self.window);
            }
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
        }
    }
}

impl Ui {
    fn new() -> Result<Self> {
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let controls =
                INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES };
            if InitCommonControlsEx(&controls) == 0 {
                bail!("Couldn't initialize Windows list controls");
            }
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: GetModuleHandleW(null()),
                lpszClassName: w!("ReasonNetworkDesktop"),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_WINDOW + 1) as _,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                bail!("Couldn't register the desktop window");
            }
            let window = CreateWindowExW(
                WS_EX_CONTROLPARENT,
                class.lpszClassName,
                w!("Reason Network Desktop"),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1160,
                760,
                null_mut(),
                null_mut(),
                class.hInstance,
                null(),
            );
            if window.is_null() {
                bail!("Couldn't create the desktop window");
            }
            // Own the window before creating children, so failures close all resources.
            let mut ui = Self {
                window,
                font: null_mut(),
                dpi: 0,
                title: null_mut(),
                status: null_mut(),
                search_label: null_mut(),
                search: null_mut(),
                working: null_mut(),
                unread: null_mut(),
                refresh: null_mut(),
                copy_address: null_mut(),
                copy_link: null_mut(),
                session_label: null_mut(),
                sessions: null_mut(),
                machine_label: null_mut(),
                machines: null_mut(),
                notice: null_mut(),
                directory: None,
                addresses: vec![],
                session_rows: vec![],
                machine_rows: vec![],
            };
            let label = |text| child(window, w!("STATIC"), text, SS_LEFT | SS_NOPREFIX, 0);
            ui.title = label("Reason Network — your team's directory")?;
            ui.status = label("Connecting to the local daemon…")?;
            ui.search_label = label("Search sessions")?;
            ui.search = child(window, w!("EDIT"), "", WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32, SEARCH)?;
            SendMessageW(ui.search, EM_SETLIMITTEXT, 256, 0);
            ui.working = child(window, w!("BUTTON"), "&Working", WS_TABSTOP | BS_AUTOCHECKBOX as u32, WORKING)?;
            ui.unread = child(window, w!("BUTTON"), "&Unread here", WS_TABSTOP | BS_AUTOCHECKBOX as u32, UNREAD)?;
            ui.refresh = child(window, w!("BUTTON"), "&Refresh", WS_TABSTOP, REFRESH)?;
            ui.copy_address = child(window, w!("BUTTON"), "Copy &address", WS_TABSTOP, COPY_ADDRESS)?;
            ui.copy_link = child(window, w!("BUTTON"), "Copy team &link", WS_TABSTOP, COPY_LINK)?;
            ui.session_label = label("Sessions — select a row to copy its exact address")?;
            ui.sessions =
                list(window, &["Address", "Agent", "Project / branch", "Activity", "Unread here", "Delivery"])?;
            ui.machine_label = label("Machines and connections")?;
            ui.machines = list(window, &["Machine", "Person", "Connection", "Latency"])?;
            ui.notice = label("Read-only directory. Messages remain in their agents' inboxes.")?;
            EnableWindow(ui.copy_link, 0);
            ui.layout();
            Ok(ui)
        }
    }

    fn layout(&mut self) {
        unsafe {
            let dpi = GetDpiForWindow(self.window).max(96);
            let scale = |v: i32| v * dpi as i32 / 96;
            let mut rect = RECT::default();
            GetClientRect(self.window, &mut rect);
            let width = (rect.right * 96 / dpi as i32 - 32).max(1);
            let height = rect.bottom * 96 / dpi as i32;
            let place = |hwnd, x, y, w: i32, h| {
                MoveWindow(hwnd, scale(x), scale(y), scale(w.max(1)), scale(h), 1);
            };
            place(self.title, 16, 14, width, 25);
            place(self.status, 16, 44, width, 38);
            place(self.search_label, 16, 90, 110, 25);
            place(self.search, 130, 86, (width - 510).max(210), 28);
            place(self.working, width - 350, 86, 100, 28);
            place(self.unread, width - 245, 86, 120, 28);
            place(self.refresh, width - 116, 86, 130, 28);
            place(self.session_label, 16, 128, width - 300, 24);
            place(self.copy_address, width - 280, 122, 140, 30);
            place(self.copy_link, width - 132, 122, 148, 30);
            let table_height = (height - 365).max(120);
            place(self.sessions, 16, 160, width, table_height);
            place(self.machine_label, 16, 170 + table_height, width, 24);
            place(self.machines, 16, 198 + table_height, width, 113);
            place(self.notice, 16, 321 + table_height, width, 38);
            for (table, widths) in
                [(self.sessions, &[260, 90, 210, 240, 95, 85][..]), (self.machines, &[300, 220, 240, 100][..])]
            {
                for (column, w) in widths.iter().enumerate() {
                    SendMessageW(table, LVM_SETCOLUMNWIDTH, column, scale(*w) as isize);
                }
            }
            if self.dpi != dpi {
                let font = CreateFontW(
                    -scale(15),
                    0,
                    0,
                    0,
                    FW_NORMAL as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    OUT_DEFAULT_PRECIS as u32,
                    CLIP_DEFAULT_PRECIS as u32,
                    CLEARTYPE_QUALITY as u32,
                    DEFAULT_PITCH as u32,
                    w!("Segoe UI"),
                );
                if !font.is_null() {
                    for hwnd in [
                        self.title,
                        self.status,
                        self.search_label,
                        self.search,
                        self.working,
                        self.unread,
                        self.refresh,
                        self.copy_address,
                        self.copy_link,
                        self.session_label,
                        self.sessions,
                        self.machine_label,
                        self.machines,
                        self.notice,
                    ] {
                        SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
                    }
                    if !self.font.is_null() {
                        DeleteObject(self.font);
                    }
                    self.font = font;
                }
                self.dpi = dpi;
            }
        }
    }

    fn action(&mut self, wp: usize, commands: &mpsc::Sender<Command>) {
        let id = wp & 0xffff;
        if id == SEARCH && (wp >> 16) == EN_CHANGE as usize || id == WORKING || id == UNREAD {
            self.render_sessions();
            return;
        }
        let command = match id {
            REFRESH => Command::Refresh {},
            COPY_LINK => Command::CopyTeamLink {},
            COPY_ADDRESS => {
                let selected =
                    unsafe { SendMessageW(self.sessions, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) };
                let Some(address) = self.addresses.get(selected as usize) else {
                    set_text(self.notice, "Select a session first.");
                    return;
                };
                Command::CopyAddress { address: address.clone() }
            }
            _ => return,
        };
        if commands.try_send(command).is_err() {
            set_text(self.notice, "A request is already pending. Try again shortly.");
        }
    }

    fn update(&mut self, update: Update) {
        match update {
            Update::Notice(text) => set_text(self.notice, text),
            Update::Copy(text, success) => set_text(
                self.notice,
                if copy(self.window, &text) { success } else { "Clipboard unavailable. Try again." },
            ),
            Update::Directory(snapshot) => {
                let online = snapshot.error.is_none();
                if let Some(d) = &snapshot.directory {
                    let unread: u64 =
                        d.sessions.iter().filter(|s| model::is_local(s, d)).map(|s| s.unread as u64).sum();
                    let machines = d.machines.iter().filter(|m| m.is_me || m.online).count();
                    set_text(
                        self.title,
                        &format!(
                            "{} — {} online machines · {} sessions · {unread} unread here",
                            d.team.as_deref().unwrap_or("Your team"),
                            machines,
                            d.sessions.len()
                        ),
                    );
                    set_text(
                        self.status,
                        if online {
                            "Live directory · refreshes every 3 seconds"
                        } else {
                            "Connection interrupted — showing the last available directory. Reconnecting…"
                        },
                    );
                    unsafe {
                        EnableWindow(self.copy_link, (online && d.team.is_some()) as i32);
                    }
                    let rows: Vec<Vec<String>> = d
                        .machines
                        .iter()
                        .map(|m| {
                            vec![
                                m.machine.clone(),
                                m.person.clone(),
                                if m.is_me {
                                    "Local".into()
                                } else if m.online {
                                    m.link.as_deref().unwrap_or("Connected").into()
                                } else {
                                    "Offline".into()
                                },
                                m.rtt_ms.map(|n| format!("{n} ms")).unwrap_or_else(|| "—".into()),
                            ]
                        })
                        .collect();
                    if rows != self.machine_rows {
                        fill(self.machines, &rows, None);
                        self.machine_rows = rows;
                    }
                } else {
                    set_text(self.status, snapshot.error.unwrap_or("Connecting…"));
                    unsafe {
                        EnableWindow(self.copy_link, 0);
                    }
                }
                self.directory = snapshot.directory;
                self.render_sessions();
            }
        }
    }

    fn render_sessions(&mut self) {
        let Some(d) = &self.directory else { return };
        let query = get_text(self.search);
        let checked = |hwnd| unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) == BST_CHECKED as isize };
        let sessions = model::visible_sessions(d, &query, checked(self.working), checked(self.unread));
        let addresses: Vec<String> = sessions.iter().map(|s| s.address.clone()).collect();
        let rows: Vec<Vec<String>> = sessions
            .iter()
            .map(|s| {
                vec![
                    s.address.clone(),
                    s.agent.clone(),
                    format!(
                        "{}{}",
                        s.repo.as_deref().unwrap_or(&s.cwd),
                        s.branch.as_ref().map(|b| format!(" / {b}")).unwrap_or_default()
                    ),
                    format!("{}{}", s.status, s.doing.as_ref().map(|t| format!(" · {t}")).unwrap_or_default()),
                    if model::is_local(s, d) { s.unread.to_string() } else { "—".into() },
                    s.tier.as_str().into(),
                ]
            })
            .collect();
        if rows != self.session_rows || addresses != self.addresses {
            let selected = unsafe { SendMessageW(self.sessions, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) };
            let selected = self.addresses.get(selected as usize).and_then(|a| addresses.iter().position(|b| a == b));
            fill(self.sessions, &rows, selected);
            self.session_rows = rows;
            self.addresses = addresses;
        }
        set_text(
            self.session_label,
            if sessions.is_empty() {
                if d.sessions.is_empty() {
                    "No sessions — open a connected coding agent"
                } else {
                    "No matching sessions — adjust search or filters"
                }
            } else {
                "Sessions — select a row to copy its exact address"
            },
        );
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn set_text(hwnd: HWND, text: &str) {
    unsafe {
        SetWindowTextW(hwnd, wide(&model::display_text(text)).as_ptr());
    }
}
fn get_text(hwnd: HWND) -> String {
    let mut text = [0u16; 257];
    let len = unsafe { GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32) };
    String::from_utf16_lossy(&text[..len.max(0) as usize])
}
fn child(parent: HWND, class: *const u16, text: &str, style: u32, id: usize) -> Result<HWND> {
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class,
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            1,
            1,
            parent,
            id as HMENU,
            GetModuleHandleW(null()),
            null(),
        )
    };
    if hwnd.is_null() {
        bail!("Couldn't create a desktop control");
    }
    Ok(hwnd)
}
fn list(parent: HWND, columns: &[&str]) -> Result<HWND> {
    let hwnd =
        child(parent, WC_LISTVIEWW, "", WS_TABSTOP | WS_BORDER | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS, 0)?;
    unsafe {
        SendMessageW(
            hwnd,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            0,
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize,
        );
        for (index, title) in columns.iter().enumerate() {
            let mut text = wide(title);
            let column =
                LVCOLUMNW { mask: LVCF_TEXT | LVCF_WIDTH, cx: 140, pszText: text.as_mut_ptr(), ..Default::default() };
            SendMessageW(hwnd, LVM_INSERTCOLUMNW, index, &column as *const _ as isize);
        }
    }
    Ok(hwnd)
}
fn fill(hwnd: HWND, rows: &[Vec<String>], selected: Option<usize>) {
    unsafe {
        let top = SendMessageW(hwnd, LVM_GETTOPINDEX, 0, 0);
        SendMessageW(hwnd, WM_SETREDRAW, 0, 0);
        SendMessageW(hwnd, LVM_DELETEALLITEMS, 0, 0);
        for (row, cells) in rows.iter().enumerate() {
            for (column, text) in cells.iter().enumerate() {
                let mut text = wide(&model::display_text(text));
                let item = LVITEMW {
                    mask: LVIF_TEXT,
                    iItem: row as i32,
                    iSubItem: column as i32,
                    pszText: text.as_mut_ptr(),
                    ..Default::default()
                };
                SendMessageW(
                    hwnd,
                    if column == 0 { LVM_INSERTITEMW } else { LVM_SETITEMW },
                    0,
                    &item as *const _ as isize,
                );
            }
        }
        if let Some(row) = selected {
            let item = LVITEMW {
                state: LVIS_SELECTED | LVIS_FOCUSED,
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                ..Default::default()
            };
            SendMessageW(hwnd, LVM_SETITEMSTATE, row, &item as *const _ as isize);
        }
        if top >= 0 && (top as usize) < rows.len() {
            SendMessageW(hwnd, LVM_ENSUREVISIBLE, top as usize, 0);
        }
        SendMessageW(hwnd, WM_SETREDRAW, 1, 0);
        InvalidateRect(hwnd, null(), 1);
    }
}

fn copy(hwnd: HWND, text: &str) -> bool {
    // CF_UNICODETEXT owns a moveable, NUL-terminated UTF-16 buffer after SetClipboardData succeeds.
    const CF_UNICODETEXT: u32 = 13;
    if text.contains('\0') {
        return false;
    }
    let text = wide(text);
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, text.len() * size_of::<u16>());
        if memory.is_null() {
            return false;
        }
        let pointer = GlobalLock(memory) as *mut u16;
        if pointer.is_null() {
            GlobalFree(memory);
            return false;
        }
        std::ptr::copy_nonoverlapping(text.as_ptr(), pointer, text.len());
        GlobalUnlock(memory);
        if OpenClipboard(hwnd) == 0 {
            GlobalFree(memory);
            return false;
        }
        let copied = EmptyClipboard() != 0 && !SetClipboardData(CF_UNICODETEXT, memory).is_null();
        CloseClipboard();
        if !copied {
            GlobalFree(memory);
        }
        copied
    }
}

async fn poll(mut commands: mpsc::Receiver<Command>, updates: mpsc::Sender<Update>) {
    let link = Link::existing();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut directory = None;
    let mut last_update = 0;
    loop {
        let command = tokio::select! {
            _ = tick.tick() => Command::Refresh {},
            command = commands.recv() => match command { Some(c) => c, None => return },
        };
        let update = match command {
            Command::Refresh {} => {
                let mut snapshot = model::read(&link).await;
                if snapshot.directory.is_some() {
                    directory = snapshot.directory.clone();
                    last_update = snapshot.updated_at;
                } else {
                    snapshot.directory = directory.clone();
                    snapshot.updated_at = last_update;
                }
                Update::Directory(Box::new(snapshot))
            }
            Command::CopyAddress { address } => {
                if model::can_copy(directory.as_ref(), &address) {
                    Update::Copy(address, "Address copied")
                } else {
                    Update::Notice("Couldn't copy this address. Refresh and try again.")
                }
            }
            Command::CopyTeamLink {} => {
                match link.call_within::<LinkInfo>(Request::Link { new: false }, Duration::from_secs(7)).await {
                    Ok(info) => Update::Copy(info.url, "Team link copied. Anyone with it can join."),
                    Err(_) => Update::Notice("Couldn't copy a team link. Check the daemon connection."),
                }
            }
        };
        if updates.send(update).await.is_err() {
            return;
        }
    }
}
