//! Where the notification area of a taskbar starts, for taskbars that have no Win32 window for
//! it (Windows 11's secondary taskbars): the left edge of the leftmost `SystemTray.*` element
//! in UI Automation. That element is the clock, or the notification bell's button or an
//! icon left of it, so the strip keeps clear of whatever the area grows.
//!
//! UI Automation calls go into Explorer's process, so they run on one worker thread, never on
//! the UI thread. The window gets a payload-free message when a position changed and reads it
//! from `SLOT`. The thread starts with the first taskbar that needs it and ends when none is
//! left. It is not joined: a call stuck in a hung Explorer must not hold up the UI thread, and
//! the process ending takes the thread with it.
//!
//! Nothing here is loaded (not even `UIAutomationCore.dll`) until a taskbar needs it.

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use windows_sys::core::{BSTR, GUID};
use windows_sys::Win32::Foundation::{SysFreeString, HWND, RECT};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows_sys::Win32::UI::Accessibility::{CUIAutomation, TreeScope_Descendants};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, PostMessageW};

use super::WM_UIA_MOVED;

/// How often the positions are read.
const POLL: Duration = Duration::from_secs(1);
const RETRY_AFTER_MISS: Duration = Duration::from_secs(10);

struct Slot {
    /// Gets `WM_UIA_MOVED`.
    main: isize,
    /// The taskbars to follow. Changing it bumps `generation`.
    bars: Vec<isize>,
    generation: u32,
    paused: bool,
    stop: bool,
    /// A worker thread exists and has not yet seen `stop` (see `worker`).
    running: bool,
    /// Per taskbar, the screen x of the tray's left edge; taskbars without a value are absent.
    left: Vec<(isize, i32)>,
}

static SLOT: Mutex<Slot> = Mutex::new(Slot {
    main: 0,
    bars: Vec::new(),
    generation: 0,
    paused: false,
    stop: true,
    running: false,
    left: Vec::new(),
});
static WAKE: Condvar = Condvar::new();

/// Follows exactly `bars` (taskbars with no notification window), starting the worker if needed
/// and stopping it if the list is empty. Forgets every earlier result.
pub fn set_targets(main: HWND, bars: &[HWND]) {
    let mut g = SLOT.lock().unwrap();
    g.main = main as isize;
    g.bars = bars.iter().map(|&h| h as isize).collect();
    g.generation = g.generation.wrapping_add(1);
    g.left.clear();
    g.stop = bars.is_empty();
    if !g.stop && !g.running {
        g.running = std::thread::Builder::new().spawn(worker).is_ok();
    }
    WAKE.notify_all();
}

/// No checks while the display is off.
pub fn set_paused(paused: bool) {
    let mut g = SLOT.lock().unwrap();
    g.paused = paused;
    WAKE.notify_all();
}

/// The screen x where `taskbar`'s tray starts, if it is known.
pub fn tray_left(taskbar: HWND) -> Option<i32> {
    let g = SLOT.lock().unwrap();
    g.left.iter().find(|&&(h, _)| h == taskbar as isize).map(|&(_, x)| x)
}

// ---- the interfaces (UIAutomationClient.h), only the methods used ----

/// IUnknown.
#[repr(C)]
struct Base {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// An owned interface pointer, released once when dropped.
struct Com<T>(*mut T);

impl<T> Drop for Com<T> {
    fn drop(&mut self) {
        // SAFETY: a live interface owning one reference; every interface starts with its
        // vtable, which starts with IUnknown.
        unsafe {
            let vtbl = *(self.0 as *const *const Base);
            ((*vtbl).release)(self.0 as *mut c_void);
        }
    }
}

#[repr(C)]
/// An interface with nothing called on it but Release (IUIAutomationCondition).
struct Unknown {
    _vtbl: *const Base,
}

#[repr(C)]
struct AutomationVtbl {
    base: Base,
    /// GetRootElement (5) is slot 5, ElementFromHandle slot 6, so slots 3..=5 come first.
    _before: [usize; 3],
    element_from_handle: unsafe extern "system" fn(*mut Automation, HWND, *mut *mut Element) -> i32,
    /// Slots 7..=20.
    _between: [usize; 14],
    create_true_condition: unsafe extern "system" fn(*mut Automation, *mut *mut Unknown) -> i32,
}

#[repr(C)]
struct Automation {
    vtbl: *const AutomationVtbl,
}

#[repr(C)]
struct ElementVtbl {
    base: Base,
    /// SetFocus, GetRuntimeId, FindFirst: slots 3..=5.
    _before: [usize; 3],
    find_all: unsafe extern "system" fn(*mut Element, i32, *mut Unknown, *mut *mut Array) -> i32,
    /// Slots 7..=29.
    _between: [usize; 23],
    get_class_name: unsafe extern "system" fn(*mut Element, *mut BSTR) -> i32,
    /// Slots 31..=42.
    _between2: [usize; 12],
    get_bounding_rectangle: unsafe extern "system" fn(*mut Element, *mut RECT) -> i32,
}

#[repr(C)]
struct Element {
    vtbl: *const ElementVtbl,
}

#[repr(C)]
struct ArrayVtbl {
    base: Base,
    get_length: unsafe extern "system" fn(*mut Array, *mut i32) -> i32,
    get_element: unsafe extern "system" fn(*mut Array, i32, *mut *mut Element) -> i32,
}

#[repr(C)]
struct Array {
    vtbl: *const ArrayVtbl,
}

/// IID_IUIAutomation.
const IID_IUIAUTOMATION: GUID = GUID::from_u128(0x30cbe57d_d9d0_452a_ab13_7ac5ac4825ee);

fn rect_of(e: &Com<Element>) -> Option<[i32; 4]> {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a live element and a valid out-pointer.
    let hr = unsafe { ((*(*e.0).vtbl).get_bounding_rectangle)(e.0, &mut r) };
    (hr >= 0).then_some([r.left, r.top, r.right, r.bottom])
}

fn is_tray_class(e: &Com<Element>) -> bool {
    let mut name: BSTR = null_mut();
    // SAFETY: a live element; the BSTR it returns is ours to free. A BSTR is NUL-terminated.
    unsafe {
        if ((*(*e.0).vtbl).get_class_name)(e.0, &mut name) < 0 || name.is_null() {
            return false;
        }
        let mut len = 0;
        while *name.add(len) != 0 {
            len += 1;
        }
        let is = std::slice::from_raw_parts(name, len).starts_with(&"SystemTray.".encode_utf16().collect::<Vec<_>>());
        SysFreeString(name);
        is
    }
}

/// Of the element rectangles in `bar`, the index of the leftmost one that is not empty and
/// lies inside the taskbar.
fn pick_leftmost(rects: &[[i32; 4]], bar: [i32; 4]) -> Option<usize> {
    rects
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r[2] > r[0] && r[3] > r[1] && r[0] >= bar[0] && r[2] <= bar[2] && r[1] < bar[3] && r[3] > bar[1]
        })
        .min_by_key(|(_, r)| r[0])
        .map(|(i, _)| i)
}

/// The leftmost `SystemTray.*` element of `taskbar` and its left edge.
fn find(auto: &Com<Automation>, taskbar: isize) -> Option<(Com<Element>, i32)> {
    let mut bar = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a valid out-pointer; a dead window fails and the lookup below fails with it.
    unsafe { GetWindowRect(taskbar as HWND, &mut bar) };
    // SAFETY: each call gets live interface pointers from the one before; every out-pointer
    // is a local, and what they return is owned by a `Com` from the moment it is set.
    unsafe {
        let a = &*(*auto.0).vtbl;
        let mut root = null_mut();
        if (a.element_from_handle)(auto.0, taskbar as HWND, &mut root) < 0 || root.is_null() {
            return None;
        }
        let root = Com(root);
        let mut cond = null_mut();
        if (a.create_true_condition)(auto.0, &mut cond) < 0 || cond.is_null() {
            return None;
        }
        let cond = Com(cond);
        let mut all = null_mut();
        if ((*(*root.0).vtbl).find_all)(root.0, TreeScope_Descendants, cond.0, &mut all) < 0 || all.is_null() {
            return None;
        }
        let all = Com(all);
        let v = &*(*all.0).vtbl;
        let mut n = 0;
        if (v.get_length)(all.0, &mut n) < 0 {
            return None;
        }
        let mut found: Vec<(Com<Element>, [i32; 4])> = Vec::new();
        for i in 0..n {
            let mut e = null_mut();
            if (v.get_element)(all.0, i, &mut e) < 0 || e.is_null() {
                continue;
            }
            let e = Com(e);
            if is_tray_class(&e) {
                if let Some(r) = rect_of(&e) {
                    found.push((e, r));
                }
            }
        }
        let rects: Vec<[i32; 4]> = found.iter().map(|f| f.1).collect();
        let i = pick_leftmost(&rects, [bar.left, bar.top, bar.right, bar.bottom])?;
        let (e, r) = found.swap_remove(i);
        Some((e, r[0]))
    }
}

struct Tracked {
    taskbar: isize,
    element: Option<(Com<Element>, i32)>,
    /// After a search that found nothing, the next one waits until then: a search walks the
    /// whole taskbar across processes, too much for every second.
    retry_at: Option<Instant>,
}

/// Reads every tracked taskbar once and returns the tray edges found.
fn poll(auto: &Com<Automation>, tracked: &mut [Tracked]) -> Vec<(isize, i32)> {
    let mut out = Vec::new();
    for t in tracked {
        // The cached element is good while it is where it was (one call); otherwise another
        // one may be leftmost now.
        if let Some((e, left)) = &t.element {
            if !rect_of(e).is_some_and(|r| r[0] == *left && r[2] > r[0]) {
                t.element = None;
            }
        }
        if t.element.is_none() && t.retry_at.is_none_or(|at| Instant::now() >= at) {
            t.element = find(auto, t.taskbar);
            t.retry_at = t.element.is_none().then(|| Instant::now() + RETRY_AFTER_MISS);
        }
        if let Some((_, left)) = &t.element {
            out.push((t.taskbar, *left));
        }
    }
    out
}

fn worker() {
    // SAFETY: balanced by CoUninitialize below when it succeeded. A multithreaded apartment
    // pumps no messages, so nothing calls back into our window procedure.
    let com = unsafe { CoInitializeEx(null(), COINIT_MULTITHREADED as u32) } >= 0;
    run();
    if com {
        // SAFETY: pairs with the successful CoInitializeEx above; every interface was
        // released when `run` returned.
        unsafe { CoUninitialize() };
    }
}

fn run() {
    let mut seen = None;
    let mut auto: Option<Com<Automation>> = None;
    let mut failed = false;
    let mut tracked: Vec<Tracked> = Vec::new();
    loop {
        let (stop, generation, paused, bars) = {
            let g = SLOT.lock().unwrap();
            (g.stop, g.generation, g.paused, g.bars.clone())
        };
        if stop {
            let mut g = SLOT.lock().unwrap();
            // Asked again between the two locks: carry on. Otherwise this is the end, and
            // `running` goes false under the same lock that new work is claimed under.
            if g.stop {
                g.running = false;
                return;
            }
            continue;
        }
        if seen != Some(generation) {
            seen = Some(generation);
            tracked = bars
                .into_iter()
                .map(|taskbar| Tracked { taskbar, element: None, retry_at: None })
                .collect();
            failed = false;
        }
        if !paused && !failed {
            if auto.is_none() {
                let mut p: *mut Automation = null_mut();
                // SAFETY: the out-pointer receives an IUIAutomation reference we own.
                let hr = unsafe {
                    CoCreateInstance(
                        &CUIAutomation,
                        null_mut(),
                        CLSCTX_INPROC_SERVER,
                        &IID_IUIAUTOMATION,
                        &mut p as *mut _ as *mut *mut c_void,
                    )
                };
                if hr >= 0 && !p.is_null() {
                    auto = Some(Com(p));
                } else {
                    failed = true;
                }
            }
            if let Some(a) = &auto {
                let now = poll(a, &mut tracked);
                let mut g = SLOT.lock().unwrap();
                // Results for a list that has been replaced since are dropped.
                if g.generation == generation && g.left != now {
                    g.left = now;
                    // SAFETY: plain post, no payload.
                    unsafe { PostMessageW(g.main as HWND, WM_UIA_MOVED, 0, 0) };
                }
            }
        }
        let g = SLOT.lock().unwrap();
        if paused || failed {
            // Nothing to do until the list, the pause or the stop changes.
            let _g = WAKE
                .wait_while(g, |g| !g.stop && g.generation == generation && (g.paused || failed))
                .unwrap();
        } else {
            let _g = WAKE
                .wait_timeout_while(g, POLL, |g| !g.stop && g.generation == generation && !g.paused)
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pick_leftmost;

    #[test]
    fn leftmost_tray_element_inside_the_taskbar() {
        let bar = [-1920, 997, 0, 1045];
        let rects = [
            [-32, 1000, -8, 1040],   // bell
            [-106, 1000, -36, 1040], // clock, moved left by the bell
            [-300, 1000, -300, 1040], // empty
            [-2500, 1000, -2400, 1040], // another monitor's
        ];
        assert_eq!(pick_leftmost(&rects, bar), Some(1));
        assert_eq!(pick_leftmost(&rects[2..], bar), None);
    }
}
