//! The settings window's keyboard behaviour, run for real: the page (with a
//! made-up host answering its calls) in headless Edge, driven by key events,
//! checking where focus goes. `settings_a11y.rs` reads the page as text;
//! this catches what that can't, like the tree losing focus when it redraws.
//!
//! Needs Microsoft Edge (on every Windows 10 and 11 PC and on GitHub's
//! Windows runners); skipped, saying so, where it isn't found or can't run,
//! unless `ONEUP_REQUIRE_EDGE` is set (CI sets it). `ONEUP_EDGE` names
//! another msedge.exe or chrome.exe.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PAGE: &str = include_str!("../src/settings_ui.html");

/// Answers the page's calls with a small made-up account: two synced
/// folders, a tree of four items (one folder with one item inside), and a
/// program in the Apps table.
const HOST: &str = r##"<script>
window.ERRS = [];
window.addEventListener("error", e => ERRS.push(e.message + " @" + e.lineno));
window.addEventListener("unhandledrejection", e => ERRS.push(String(e.reason && e.reason.message || e.reason)));
const HOST_LISTENERS = [];
const HOST_ANSWERS = {
  status: null, stopped: null,
  snapshot: a => a && a.root ? {rules: {presets: [], exclude: [], paused: [], cloud: "pull"}, priority: {first: [], last: []},
      databases: {wait: true, presets: [], patterns: [], keep_syncing: []},
      space: {free_unused_days: 0, low_disk_percent: 10, free_when_low: false}} :
    {pair: {local: "C:\\Sync"}, agent: {memory: "balanced"}, versions: {}, safety: {max_changes: 200, max_percent: 10, content_guard: 20},
      updates: {auto: true, channel: "stable"}, onedrive_app: {folders: [], top_files: false}, office_opener: {registered: false, defaults: []},
      search: {onedrive: false, hotkey: ""}, setup: {allowed: true}, tuning: [], version: "0.0.0", data_dir: "C:\\Data",
      autostart: false, signed_in: true,
      sync: {interval_minutes: 15, keep_push: false},
      network: {this: {id: "{11111111-2222-3333-4444-555555555555}", name: "Home Wi-Fi", metered: false, connectivity: "internet"},
        own: {transfer: "small", small_mb: 5, upload_kib: null, download_kib: null, interval_minutes: null, keep_push: "on", budget_mb: null},
        metered: {transfer: "paused", small_mb: null, upload_kib: null, download_kib: null, interval_minutes: null, keep_push: null, budget_mb: 2000},
        small_mb_default: 10}},
  roots: [{id: "main", kind: "main", local: "C:\\Sync", remote: "/", on_demand: true, running: true},
          {id: "pair:Photos", kind: "pair", name: "Photos", local: "D:\\Photos", remote: "/Photos", running: true}],
  tree: a => ({more: 0, items: a.rel === "" ? [
      {name: "Docs", folder: true, state: "synced", local_id: 1, remote_id: "r1", here: true, cloud: true, path: "C:\\Sync\\Docs"},
      {name: "Music", folder: true, state: "synced", partial: true, local_id: 2, remote_id: "r2", here: true, cloud: true, path: "C:\\Sync\\Music"},
      {name: "a.txt", folder: false, state: "left_out", why: {kind: "own", rule: "/a.txt"}, size: 10, here: true, path: "C:\\Sync\\a.txt"},
      {name: "big.iso", folder: false, state: "synced", size: 4500000000, here: true, cloud: true, path: "C:\\Sync\\big.iso"}]
    : [{name: "inner.doc", folder: false, state: "synced", size: 1830, here: true, cloud: true, path: "C:\\Sync\\Docs\\inner.doc"}]}),
  tree_sizes: {local: {}, remote: {}}, tree_order: "normal", activity: [], kept: [], problems: {problems: []},
  shortcuts: {found: [], last_fix: null}, tree_preview: {mode: "pull", token: "t", here: {files: 1, bytes: 10}},
  apps: {series: [{name: "Word", counts: [1, 2, 0]}], apps: [{name: "Word", exe: "winword.exe", changed: 3, opened: 1, bytes: 1830}],
    start_ms: 0, hourly: false},
  app_files: [], rule_suggestions: {largest: [], busiest: []}, rule_files: [],
  // Recovery: a rewind of 600 changes (two pages), one deleted item.
  rewind_state: {since_ms: 1, job: null, code: null},
  rewind_preview: a => ({folder: "/Docs", at_ms: 5000, code: "0123456789abcdef", started: false, skipped: [],
    changes: Array.from({length: 600}, (_, i) => ({kind: "Revert", folder: false, then: "/Docs/f" + i, now: "/Docs/f" + i,
      size: 10, source: "Versions"}))}),
  rewind_start: a => ({...HOST_ANSWERS.rewind_preview(a), started: true}),
  // The folder changed before the approval: another list comes back.
  rewind_approve: a => ({...HOST_ANSWERS.rewind_preview(a), code: "fedcba9876543210", approved: false, changed: true}),
  rewind_cancel: "", conflicts: [],
  cloud_bin: [{seq: 7, at_ms: 5000, path: "/Docs/gone.txt", folder: false, size: 3, restorable: true}],
  cloud_bin_restore: "/Docs/gone.txt",
};
window.chrome = {webview: {
  postMessage(text) {
    const m = JSON.parse(text);
    let v = HOST_ANSWERS[m.op];
    if (typeof v === "function") v = v(m.args);
    if (v === undefined) v = {};
    setTimeout(() => HOST_LISTENERS.forEach(f => f({data: {id: m.id, ok: v}})), 1);
  },
  addEventListener(type, f) { HOST_LISTENERS.push(f); }
}};
</script>
"##;

/// Presses keys and records what happened, then writes the checks into
/// `<pre id="result">` as JSON: `[[name, passed, what was seen], …]`.
const DRIVER: &str = r##"<script>
(async () => {
  const out = [];
  const pre = document.createElement("pre");
  pre.id = "result";
  document.body.append(pre);
  const report = () => pre.textContent = JSON.stringify(out.concat([["no script errors", !ERRS.length, ERRS.join("; ")]]));
  const check = (name, ok, seen) => { out.push([name, !!ok, String(seen)]); report(); };
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const key = (k, extra = {}) => document.activeElement.dispatchEvent(new KeyboardEvent("keydown", {key: k, bubbles: true, cancelable: true, ...extra}));
  const at = () => { const a = document.activeElement; return a && a.dataset && a.dataset.rel != null ? a.dataset.rel : a ? a.tagName + "#" + a.id : "none"; };
  const row = rel => document.querySelector(`#tree [data-rel="${rel}"]`);
  try {
    await sleep(600);
    const tree = document.querySelector("#tree");
    check("the tree has its rows", tree.querySelectorAll("[role=treeitem][data-rel]").length === 4, tree.querySelectorAll("[data-rel]").length);
    check("one tab stop in the tree", tree.querySelectorAll("[tabindex='0']").length === 1, tree.querySelectorAll("[tabindex='0']").length);
    tree.querySelector("[tabindex='0']").focus();
    await sleep(50);
    check("focusing a row keeps focus on it (it redraws)", at() === "Docs", at());
    check("the focused row is selected", row("Docs") && row("Docs").getAttribute("aria-selected") === "true", at());
    key("ArrowRight"); await sleep(300);
    check("Right opens a folder and keeps focus", at() === "Docs" && row("Docs").getAttribute("aria-expanded") === "true", at());
    key("ArrowDown"); await sleep(100);
    check("Down goes into the folder", at() === "Docs/inner.doc", at());
    check("selection follows focus", row("Docs/inner.doc").getAttribute("aria-selected") === "true" && tree.querySelectorAll("[aria-selected=true]").length === 1, at());
    key("ArrowLeft"); await sleep(100);
    check("Left goes to the parent", at() === "Docs", at());
    key("End"); await sleep(100);
    check("End goes to the last item", at() === "big.iso", at());
    key("Home"); await sleep(100);
    check("Home goes to the first item", at() === "Docs", at());
    key("a"); await sleep(100);
    check("a letter goes to the next item starting with it", at() === "a.txt", at());
    check("the box is the item's checked state", row("a.txt").getAttribute("aria-checked") === "false" && row("Music").getAttribute("aria-checked") === "mixed", row("Music").getAttribute("aria-checked"));

    // Space on a synced item: the exclude preview, and focus back after.
    row("Docs").focus(); await sleep(50);
    key(" "); await sleep(300);
    const dlg = document.querySelector("#dlg-exclude");
    check("Space opens the exclude preview", dlg.open && dlg.contains(document.activeElement), at());
    // The tree redraws meanwhile (sync went on): focus goes to the item's new row.
    renderTree();
    dlg.close(); await sleep(100);
    check("closing the dialog gives focus back to the row, even redrawn", at() === "Docs", at());

    // The right-click menu from the keyboard.
    key("ContextMenu"); await sleep(150);
    const ctx = document.querySelector("#ctx");
    const items = [...ctx.querySelectorAll("button")];
    check("the menu key opens the menu on its first item", ctx.matches(":popover-open") && document.activeElement === items[0], at());
    key("ArrowDown"); await sleep(50);
    check("Down moves in the menu", document.activeElement === items[1], document.activeElement.textContent);
    key("End"); await sleep(50);
    check("End goes to the menu's last item", document.activeElement === items[items.length - 1], document.activeElement.textContent);
    key("ArrowDown"); await sleep(50);
    check("Down from the last item wraps to the first", document.activeElement === items[0], document.activeElement.textContent);
    renderTree();
    ctx.hidePopover(); await sleep(150);
    check("closing the menu gives focus back to the row", at() === "Docs", at());

    // The folder tabs: one tab stop; the arrows move and choose.
    const tabs = () => [...document.querySelectorAll("#roots [role=tab]")];
    check("one tab stop among the folder tabs", tabs().filter(t => t.tabIndex === 0).length === 1, tabs().map(t => t.tabIndex));
    tabs().find(t => t.tabIndex === 0).focus();
    key("ArrowRight"); await sleep(400);
    check("Right chooses the next folder and keeps focus on it",
      document.activeElement.getAttribute("aria-selected") === "true" && document.activeElement.textContent.includes("Photos")
        && tabs().filter(t => t.tabIndex === 0).length === 1, document.activeElement.textContent);
    key("Home"); await sleep(400);
    check("Home chooses the first folder", document.activeElement.textContent.includes("Main folder") && document.activeElement.getAttribute("aria-selected") === "true", document.activeElement.textContent);

    // The views.
    const views = [...document.querySelectorAll(".views [role=tab]")];
    views.find(v => v.tabIndex === 0).focus();
    key("ArrowRight"); await sleep(300);
    check("Right chooses Exclude", document.activeElement.dataset.view === "out" && document.activeElement.getAttribute("aria-selected") === "true", document.activeElement.dataset.view);
    check("the list isn't called a tree when it isn't one", tree.getAttribute("role") === "region", tree.getAttribute("role"));
    key("ArrowLeft"); await sleep(300);
    check("Left goes back to All files", document.activeElement.dataset.view === "all" && tree.getAttribute("role") === "tree", document.activeElement.dataset.view);

    // The status menu, opened by its button.
    document.querySelector("#st-btn").focus();
    document.querySelector("#st-btn").click(); await sleep(150);
    const st = [...document.querySelectorAll("#st-menu button")];
    check("the status menu opens on its first item", document.querySelector("#st-menu").matches(":popover-open") && document.activeElement === st[0], at());
    key("ArrowUp"); await sleep(50);
    check("Up from the first item wraps to the last", document.activeElement === st[st.length - 1], document.activeElement.textContent);
    document.querySelector("#st-menu").hidePopover(); await sleep(150);
    check("closing it gives focus back to its button", document.activeElement.id === "st-btn", at());

    // Apps: a program opens its files from the keyboard, and keeps focus across reloads.
    document.querySelector('nav [data-page="apps"]').click(); await sleep(400);
    const prog = document.querySelector("#apps-table .cell-btn");
    check("a program is a button", prog && prog.getAttribute("aria-expanded") === "false", prog && prog.outerHTML);
    prog.focus(); prog.click(); await sleep(300);
    check("it opens its files", document.querySelector("#apps-table .cell-btn").getAttribute("aria-expanded") === "true", "");
    await loadApps(true); await sleep(100);
    check("a reload keeps focus on it", document.activeElement.classList.contains("cell-btn"), at());

    // Network: the sync interval, this network's settings and the metered
    // ones are filled in and reachable from the keyboard.
    document.querySelector('nav [data-page="network"]').click(); await sleep(300);
    check("the sync interval is shown", document.querySelector('[data-sync="interval_minutes"]').value === "15",
      document.querySelector('[data-sync="interval_minutes"]').value);
    check("this network is named", document.querySelector("#net-name").textContent === "Home Wi-Fi", document.querySelector("#net-name").textContent);
    check("its own settings are shown", document.querySelector('[data-net-this="transfer"]').value === "small"
      && document.querySelector('[data-net-this="keep_push"]').value === "on" && !document.querySelector("#net-forget").hidden,
      document.querySelector('[data-net-this="transfer"]').value);
    check("the metered ones too", document.querySelector('[data-net-metered="transfer"]').value === "paused"
      && document.querySelector('[data-net-metered="budget_mb"]').value === "2000", document.querySelector('[data-net-metered="budget_mb"]').value);
    for (const sel of ['[data-sync="interval_minutes"]', '[data-net-this="transfer"]', '[data-net-metered="upload_kib"]']) {
      const e = document.querySelector(sel);
      e.focus();
      check(sel + " takes focus", document.activeElement === e, at());
    }
    check("Work offline waits for the agent to run", document.querySelector("#net-offline").disabled, "");
    // Recovery: focus follows the rewind's list and answers.
    window.confirm = () => true;
    document.querySelector('nav [data-page="recovery"]').click(); await sleep(300);
    document.querySelector("#rw-at").value = "2026-01-01T10:00:00";
    document.querySelector("#rw-preview").focus();
    document.querySelector("#rw-preview").click(); await sleep(300);
    const more = document.querySelector("#rw-actions button");
    check("a long list shows Show more, not Rewind", more && more.textContent === "Show more", more && more.textContent);
    more.focus(); more.click(); await sleep(100);
    check("Show more gives focus to what replaces it", document.activeElement.textContent === "Rewind", at());
    document.activeElement.click(); await sleep(300);
    check("asking for the rewind gives focus to Approve", document.activeElement.textContent === "Approve", at());
    document.activeElement.click(); await sleep(300);
    check("a list that changed before the approval is shown again, from its start",
      document.activeElement.textContent === "Show more" && !document.querySelector("#rw-actions").textContent.includes("Approve"), at());
    document.activeElement.click(); await sleep(100);
    check("the new list is offered for approval once all of it is shown",
      document.activeElement.textContent === "Approve" && document.querySelector("#rw-actions").textContent.includes("fedcba9876543210"), at());
    const cancel = [...document.querySelectorAll("#rw-actions button")].find(b => b.textContent === "Cancel");
    cancel.focus(); cancel.click(); await sleep(300);
    check("cancelling gives focus back to Preview", document.activeElement.id === "rw-preview", at());
    const restore = document.querySelector("#rc-bin button");
    check("a deleted item's Restore names it", restore && restore.getAttribute("aria-label") === "Restore /Docs/gone.txt", restore && restore.outerHTML);
    restore.focus(); restore.click(); await sleep(300);
    check("restoring keeps focus in the list as it reloads", document.querySelector("#rc-bin").contains(document.activeElement), at());
  } catch (e) {
    check("the driver ran to the end", false, e.message + " " + e.stack);
  }
  check("done", true, "");
})();
</script>
"##;

/// Edge and the temp folder it runs in: on drop (a failed check included)
/// Edge is killed if it still runs and waited for, and the folder removed.
struct Run {
    child: Option<Child>,
    dir: PathBuf,
}

impl Run {
    /// Waits for Edge to end by itself, at most `limit`; whether it did.
    fn wait(&mut self, limit: Duration) -> bool {
        let Some(child) = self.child.as_mut() else { return true };
        let started = Instant::now();
        while started.elapsed() < limit {
            if child.try_wait().ok().flatten().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }

    /// Ends Edge and removes the folder, saying if it couldn't.
    fn finish(mut self) -> std::io::Result<()> {
        self.stop();
        remove_dir(&self.dir)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stop();
        let _ = remove_dir(&self.dir);
    }
}

/// Removes a folder, trying again for a few seconds: Edge's helper
/// processes let go of its profile shortly after Edge itself ends.
fn remove_dir(dir: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::remove_dir_all(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if tries >= 40 => return Err(e),
            Err(_) => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(250));
            }
            Ok(()) => return Ok(()),
        }
    }
}

fn edge() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ONEUP_EDGE") {
        return Some(PathBuf::from(p));
    }
    let mut places = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            places.push(PathBuf::from(base).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
    }
    places.into_iter().find(|p| p.exists())
}

#[test]
fn the_window_works_from_the_keyboard() {
    let Some(edge) = edge() else {
        eprintln!("Microsoft Edge isn't installed (or set ONEUP_EDGE): the keyboard test is skipped");
        return;
    };
    let dir = std::env::temp_dir().join(format!("oneup-keyboard-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temp folder");
    let mut run = Run { child: None, dir: dir.clone() };
    let page = PAGE
        .replace("{{START_PAGE}}", "")
        .replace("\"{{START_ITEM}}\"", "\"\"")
        .replacen("<script>\n        \"use strict\";", &format!("{HOST}<script>\n        \"use strict\";"), 1)
        .replacen("</body>", &format!("{DRIVER}</body>"), 1);
    assert!(page.contains("HOST_ANSWERS") && page.contains("id = \"result\""), "the page's script start or end moved");
    let file = dir.join("page.html");
    std::fs::write(&file, page).expect("the page");
    let url = format!("file:///{}", file.display().to_string().replace('\\', "/"));
    // Into a file: Edge writes nothing to a pipe.
    let dump = dir.join("dom.html");
    run.child = Some(
        Command::new(&edge)
            .args(["--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check"])
            .arg(format!("--user-data-dir={}", dir.join("profile").display()))
            .args(["--virtual-time-budget=60000", "--dump-dom", &url])
            .stdout(Stdio::from(std::fs::File::create(&dump).expect("the output file")))
            .stderr(Stdio::null())
            .spawn()
            .expect("starting Edge"),
    );
    assert!(run.wait(Duration::from_secs(120)), "Edge didn't finish in 120 s");
    let dom = std::fs::read_to_string(&dump).unwrap_or_default();
    run.finish().unwrap_or_else(|e| panic!("removing {}: {e}", dir.display()));
    // Edge that can't run here (a sandbox that stops its helper processes)
    // prints nothing at all; CI sets ONEUP_REQUIRE_EDGE so it can't pass so.
    if dom.is_empty() {
        let say = "Edge printed nothing (it can't run headless here): the keyboard test is skipped";
        assert!(std::env::var_os("ONEUP_REQUIRE_EDGE").is_none(), "{say}, but ONEUP_REQUIRE_EDGE is set");
        eprintln!("{say}");
        return;
    }
    let json = dom
        .split("<pre id=\"result\">")
        .nth(1)
        .and_then(|r| r.split("</pre>").next())
        .unwrap_or_else(|| panic!("no result from the page; Edge printed {} bytes", dom.len()));
    let json = json.replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
    let checks: Vec<(String, bool, String)> = serde_json::from_str(&json).expect("the checks");
    let failed: Vec<String> =
        checks.iter().filter(|(_, ok, _)| !ok).map(|(name, _, seen)| format!("{name} (saw: {seen})")).collect();
    assert!(checks.iter().any(|(n, ..)| n == "done"), "the checks didn't finish: {checks:?}");
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    assert!(checks.len() > 25, "only {} checks ran", checks.len());
}
