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
const HOST_CALLS = [];
const HOST_ANSWERS = {
  status: null, stopped: null,
  setup_items: a => ({items: a.side === "r" ? [{key:"r:old",path:"/cloud.txt",folder:false,selected:true}]
    : [{key:"l:1",path:"/local.txt",folder:false,selected:false}],next:null}),
  setup_select: null,
  shared_settings_preview: {code:"reviewed-settings",enabled:false,
    local:{scope:"/",rules:{presets:[],exclude:[]},sync:{interval_minutes:0,keep_push:false},databases:{wait:true,presets:[],patterns:[]}},
    cloud:{scope:"/",rules:{presets:["dev"],exclude:["build/"]},sync:{interval_minutes:15,keep_push:true},databases:{wait:true,presets:[],patterns:[]}}},
  shared_settings_import: null, shared_settings_publish:null, shared_settings_disable:null,
  setup_preview: {token: "abc", text: "First page\n", next: 11},
  setup_preview_page: a => a.offset === 11 ? {token: "abc", text: "Last page\n", next: null}
    : {token: "abc", text: "First page\n", next: 11},
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
  unsynced: {items: ["/1.", "/bad?.txt"].map(path => ({path, kind: "invalid_name", why: "Name not allowed",
    detail: "OneDrive doesn't allow this name; rename it", subject: "l:" + path, here: true,
    choices: [{code: "rename", label: "Rename", bulk: false}, {code: "repair-names", label: "Repair", bulk: true}]}))},
  unsynced_preview: {items: 2, tops: ["/1.", "/bad?.txt"], renames: [{path: "/1.", name: "1"}, {path: "/bad?.txt", name: "bad_.txt"}],
    refused: ["/busy.: an answer is already queued"], code: "repair-preview-code"},
  unsynced_resolve: "Queued: repair names for 2 items.",
  rewind_cancel: "", conflicts: [],
  cloud_bin: [{seq: 7, at_ms: 5000, path: "/Docs/gone.txt", folder: false, size: 3, restorable: true}],
  cloud_bin_restore: "/Docs/gone.txt",
  item_history: a => a.before ? [{seq: 1, at_ms: 1000, outcome: "done", action: "upload", path: "/Docs/old.txt", detail: "Older edit"}]
    : Array.from({length: 100}, (_, i) => ({seq: 101-i, at_ms: 5000, outcome: "done", action: "upload", path: "/Docs/a.txt", detail: "Saved edit"})),
  item_versions: {versions: [{id: "v1", modified_ms: 2000, size: 3, current: false}]},
};
window.chrome = {webview: {
  postMessage(text) {
    const m = JSON.parse(text);
    HOST_CALLS.push(m);
    let v = HOST_ANSWERS[m.op];
    if (typeof v === "function") v = v(m.args);
    if (v === undefined) v = {};
    Promise.resolve(v).then(answer => setTimeout(() => HOST_LISTENERS.forEach(f => f({data: {id: m.id, ok: answer}})), 1));
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
    // Notifications have independent spacing and only reviewed incidents can be dismissed.
    const savedProblems = HOST_ANSWERS.problems;
    HOST_ANSWERS.problems = {problems: [
      {kind:"live_database", title:"Database waiting: /" + "long-path/".repeat(30), detail:"Database has pending changes.", action:"Close the program."},
      {kind:"edit_not_found", title:"Review cloud version history", detail:"An overlapping cloud edit could not be checked.", action:"Review versions, then dismiss.", data_dir:"C:\\Data", dismiss_code:"incident-1"}
    ]};
    show("lists"); await loadAttention();
    const notices = document.querySelectorAll("#attn > .card");
    const activityTabs = document.querySelector("#ls-tabs");
    check("notifications have separate cards and space before list controls", notices.length === 2 && notices[1].getBoundingClientRect().top - notices[0].getBoundingClientRect().bottom >= 9 && activityTabs.getBoundingClientRect().top - notices[1].getBoundingClientRect().bottom >= 15, notices.length);
    check("long notification paths stay inside their card", notices[0].querySelector(".label").scrollWidth <= notices[0].querySelector(".label").clientWidth + 1, notices[0].scrollWidth);
    check("only version-review notices have Dismiss", document.querySelectorAll("#attn button").length === 1, document.querySelectorAll("#attn button").length);
    HOST_ANSWERS.dismiss_problem = () => { HOST_ANSWERS.problems = {problems: [HOST_ANSWERS.problems.problems[0]]}; return null; };
    document.querySelector("#attn button").click(); await sleep(100);
    const dismissal = HOST_CALLS.filter(m => m.op === "dismiss_problem").pop();
    check("Dismiss acknowledges exactly this incident and leaves active waits", dismissal?.args.code === "incident-1" && dismissal.args.data_dir === "C:\\Data" && document.querySelectorAll("#attn > .card").length === 1, JSON.stringify(dismissal));
    HOST_ANSWERS.problems = {problems: []};
    document.querySelector("#ls-refresh").click(); await sleep(100);
    check("Activity Refresh also refreshes notifications", document.querySelector("#attn").hidden, document.querySelector("#attn").textContent);
    let oldAttention;
    HOST_ANSWERS.problems = () => new Promise(resolve => oldAttention = resolve);
    const pendingAttention = loadAttention();
    HOST_ANSWERS.problems = {problems: []};
    await loadAttention();
    oldAttention({problems: [{title:"Old notice", detail:"Dismissed", action:"Review"}]});
    await pendingAttention;
    check("a stale reply cannot resurrect a dismissed notification", document.querySelector("#attn").hidden && !document.querySelector("#attn").children.length, document.querySelector("#attn").textContent);
    HOST_ANSWERS.problems = savedProblems;
    show("files"); await sleep(200);
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

    // History opens from a folder's menu, pages activity and scopes local copies.
    HOST_ANSWERS.kept = [
      {id: "local1", origin: "Docs/kept.txt", name: "kept.txt", at_ms: 3000, bytes: 4},
      {id: "local2", origin: "Docs2/unrelated.txt", name: "unrelated.txt", at_ms: 3000, bytes: 4}];
    key("ContextMenu"); await sleep(100);
    [...ctx.querySelectorAll("button")].find(b => b.textContent === "View history").click();
    await sleep(300);
    const history = [...document.querySelectorAll("#details details")].find(d => d.querySelector("summary").textContent === "History");
    check("View history opens the history panel", history && history.open, history && history.textContent);
    check("history shows local copies and recorded cloud deletions", history.textContent.includes("kept.txt") && history.textContent.includes("gone.txt") && !history.textContent.includes("unrelated.txt"), history.textContent);
    const historyMore = [...history.querySelectorAll("button")].find(b => b.textContent === "Show more");
    historyMore.click(); await sleep(150);
    check("history can page back to older activity", history.textContent.includes("Older edit") && historyMore.hidden, history.textContent);
    check("history matching respects folder boundaries", historyMatches("Docs/a", {rel: "Docs", folder: true}) && !historyMatches("Docs2/a", {rel: "Docs", folder: true}) && !historyMatches("Docs/a", {rel: "Docs", folder: false}), "");
    const fileHistory = document.createElement("div");
    await historyInto(fileHistory, {rel: "Docs/inner.doc", path: "C:\\Sync\\Docs\\inner.doc", folder: false, cloud: true});
    await sleep(150);
    check("file history offers cloud version restore", fileHistory.textContent.includes("OneDrive versions") && [...fileHistory.querySelectorAll("button")].some(b => b.textContent === "Restore"), fileHistory.textContent);

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

    // Settings, at the foot of the list: its pages replace the list, and Back returns.
    const before = document.querySelector(".page.on").id;
    const gear = document.querySelector("#nav-open-settings");
    gear.focus(); gear.click(); await sleep(100);
    check("Settings shows the settings pages", document.querySelector("#nav-app").hidden
      && !document.querySelector("#nav-settings").hidden && document.querySelector("#general").classList.contains("on"), "");
    check("focus goes to Back", document.activeElement.id === "nav-back", at());
    document.activeElement.click(); await sleep(100);
    check("Back returns to the page before", !document.querySelector("#nav-app").hidden
      && document.querySelector(".page.on").id === before, document.querySelector(".page.on").id);
    check("focus goes back to Settings", document.activeElement.id === "nav-open-settings", at());

    // Bulk repair is a before/after preview, never a silent rename.
    document.querySelector('nav [data-page="lists"]').click(); await sleep(200);
    document.querySelector('[data-tab="unsynced"]').click(); await sleep(300);
    const repairButton = () => [...document.querySelectorAll("#lists button")].find(b => b.textContent === "Repair names");
    check("invalid names offer bulk repair", !!repairButton(), document.querySelector("#lists").textContent);
    repairButton().click(); await sleep(200);
    const repairDialog = document.querySelector("#dlg-resolve");
    check("repair shows every proposed new name", repairDialog.open && document.querySelectorAll("#rs-repairs tbody tr").length === 2
      && document.querySelector("#rs-repairs").textContent.includes("bad_.txt"), repairDialog.textContent);
    check("repair shows skipped items", document.querySelector("#rs-facts").textContent.includes("/busy."), repairDialog.textContent);
    check("preview doesn't queue anything", !HOST_CALLS.some(m => m.op === "unsynced_resolve"), "");
    repairDialog.querySelector('[value="cancel"]').click(); await sleep(100);
    check("cancel doesn't queue anything", !HOST_CALLS.some(m => m.op === "unsynced_resolve"), "");
    repairButton().click(); await sleep(200);
    document.querySelector("#rs-go").click(); await sleep(300);
    const queuedRepair = HOST_CALLS.find(m => m.op === "unsynced_resolve");
    check("repair confirms exactly the previewed batch", queuedRepair && queuedRepair.args.choice === "repair-names"
      && queuedRepair.args.all && queuedRepair.args.expect === "repair-preview-code", JSON.stringify(queuedRepair));
    check("repair closes after queueing", !repairDialog.open, repairDialog.open);

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
    // This network and Metered connections start folded; each opens from its heading.
    for (const id of ["#net-this", "#net-metered"]) {
      const d = document.querySelector(id), sum = d.querySelector("summary");
      check(id + " starts folded", !d.open, "");
      sum.focus();
      check(id + "'s heading takes focus", document.activeElement === sum, at());
      sum.click(); await sleep(50);
      check(id + " opens", d.open, "");
    }
    for (const sel of ['[data-sync="interval_minutes"]', '[data-net-this="transfer"]', '[data-net-metered="upload_kib"]']) {
      const e = document.querySelector(sel);
      e.focus();
      check(sel + " takes focus", document.activeElement === e, at());
    }
    check("Work offline waits for the agent to run", document.querySelector("#net-offline").disabled, "");
    // Recovery: focus follows the rewind's list and answers.
    window.confirm = () => true;
    document.querySelector('nav [data-page="recovery"]').click(); await sleep(300);
    document.querySelector("#rw-date").value = "2026-01-01";
    document.querySelector("#rw-time").value = "10:00";
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
    document.querySelector("#nav-open-settings").click();
    document.querySelector('nav button[data-page="general"]').click(); await sleep(300);
    const sharedDescription = document.querySelector("#shared-settings>.note").getBoundingClientRect();
    const sharedActions = document.querySelector("#shared-settings>.actions").getBoundingClientRect();
    const sharedAutomatic = document.querySelector("#shared-settings>.ck").getBoundingClientRect();
    check("shared settings have space around their action row", sharedActions.top - sharedDescription.bottom >= 16
      && sharedAutomatic.top - sharedActions.bottom >= 16, "");
    const beforeSetupCalls = HOST_CALLS.length;
    document.querySelector("#setup-rerun").focus(); document.activeElement.click(); await sleep(100);
    check("Updates reopens setup and focuses the current local folder", document.querySelector("#setup").classList.contains("on")
      && document.activeElement.id === "setup-local" && document.activeElement.value === "C:\\Sync", at());
    check("reopening retains saved exclusions and needs another preview", document.querySelector("#setup-preset").value === "imported"
      && document.querySelector("#start-btn").disabled, "");
    check("reopening makes no host mutations", HOST_CALLS.slice(beforeSetupCalls).every(m => !["setup_start", "setup_select", "sign_out", "stop"].includes(m.op)), "");
    check("setup contains the shared settings controls", document.querySelector("#setup-shared-slot #shared-settings"), "");
    document.querySelector("#preview-btn").click(); await sleep(300);
    check("setup shows the first preview page", document.querySelector("#preview").textContent === "First page\n", "");
    check("setup has another page", !document.querySelector("#preview-next").disabled, "");
    document.querySelector("#preview-next").click(); await sleep(300);
    check("setup can reach the last preview page", document.querySelector("#preview").textContent === "Last page\n"
      && document.querySelector("#preview-next").disabled, "");
    document.querySelector("#preview-prev").click(); await sleep(300);
    check("setup can return to the first page", document.querySelector("#preview").textContent === "First page\n"
      && document.querySelector("#preview-prev").disabled, "");
    const localChoice = document.querySelector("#setup-choice-list input");
    check("initial local-only file starts unselected", localChoice && !localChoice.checked, "");
    localChoice.click(); await sleep(300);
    check("choosing an upload records that item and requires a new preview", HOST_CALLS.some(m=>m.op==="setup_select" && m.args.key==="l:1" && m.args.selected)
      && document.querySelector("#start-btn").disabled, "");
    document.querySelector("#shared-review").click(); await sleep(300);
    check("portable settings are compared before import", document.querySelector("#shared-comparison").textContent.includes("build/")
      && !HOST_CALLS.some(m=>m.op==="shared_settings_import"), "");
    check("automatic publication needs opt-in", !document.querySelector("#shared-auto").checked, "");
    document.querySelector("#shared-import").click(); await sleep(500);
    check("import binds the reviewed document", HOST_CALLS.some(m=>m.op==="shared_settings_import" && m.args.code==="reviewed-settings" && !m.args.automatic), "");

    // Switching folders while the old folder's replies are still in flight.
    const savedTree = HOST_ANSWERS.tree, savedSnapshot = HOST_ANSWERS.snapshot;
    let oldTree;
    HOST_ANSWERS.tree = a => a.root === "main" ? new Promise(resolve => oldTree = resolve)
      : {items:[{name:"photo.jpg",folder:false,state:"synced",here:true}],more:0};
    const pendingTree = selectRoot("main");
    await sleep(50);
    await selectRoot("pair:Photos");
    oldTree({items:[{name:"stale.txt",folder:false,state:"synced",here:true}],more:0});
    await pendingTree;
    check("a previous folder's listing cannot populate the selected folder", F.root.id === "pair:Photos"
      && F.nodes.has("photo.jpg") && !F.nodes.has("stale.txt"), [...F.nodes.keys()]);
    HOST_ANSWERS.tree = savedTree;
    let oldSnapshot;
    HOST_ANSWERS.snapshot = a => a && a.root === "main" ? new Promise(resolve => oldSnapshot = resolve)
      : {...savedSnapshot(a), marker:"photos"};
    const pendingSnapshot = selectRoot("main");
    await sleep(50);
    await selectRoot("pair:Photos");
    oldSnapshot({...savedSnapshot({root:"main"}), marker:"stale"});
    await pendingSnapshot;
    check("a previous folder's settings cannot replace the selected folder's settings", F.rs.marker === "photos", F.rs.marker);
    HOST_ANSWERS.snapshot = savedSnapshot;
    let oldDuplicates;
    HOST_ANSWERS.duplicates = a => a.root === "main" ? new Promise(resolve => oldDuplicates = resolve)
      : {page:[{marker:"photos"}],next:null};
    await selectRoot("main");
    const pendingDuplicates = loadDupes(false);
    await sleep(50);
    await selectRoot("pair:Photos");
    await loadDupes(false);
    oldDuplicates({page:[{marker:"stale"}],next:null});
    await pendingDuplicates;
    check("a previous folder's duplicates cannot replace the selected folder's results", D.root === "pair:Photos"
      && D.groups.length === 1 && D.groups[0].marker === "photos", JSON.stringify(D.groups));
    const orders = [];
    const orderControl = seg([["first","First"],["normal","Normal"],["last","Last"]], "normal", v => orders.push(v));
    const orderButtons = orderControl.querySelectorAll("button");
    orderButtons[0].click(); orderButtons[1].click(); orderButtons[1].click();
    check("sync order can return to its original selection without reopening details", orders.join(",") === "first,normal"
      && orderButtons[1].getAttribute("aria-pressed") === "true", orders);
    // Vault credentials use masked fields, clear on close, and never send a cancelled action.
    const focusBack = document.createElement("button"); document.body.append(focusBack); focusBack.focus();
    const secretDialog = document.querySelector("#dlg-secret"), secretInput = document.querySelector("#secret-password");
    const secretConfirm = document.querySelector("#secret-confirm"), secretForm = secretDialog.querySelector("form");
    const secretSubmit = document.querySelector("#secret-go");
    const enterSecret = (input, value) => { input.value = value; input.dispatchEvent(new Event("input", {bubbles:true})); };
    const vaultBefore = HOST_CALLS.filter(m=>m.op.startsWith("vault_")).length;
    const cancelledUnlock = unlockVault({name:"Test vault"});
    check("vault unlock uses a focused masked field without a save-password option", secretDialog.open && secretInput.type === "password"
      && document.activeElement === secretInput && !document.querySelector("#secret-remember"), secretDialog.open);
    check("Enter submits the vault action rather than Cancel", secretForm.querySelector(":default") === secretSubmit, "");
    enterSecret(secretInput, "test-only-secret"); secretDialog.requestClose(); await sleep(50); await cancelledUnlock;
    check("Esc cancels vault unlock, clears credentials and restores focus", !secretInput.value && document.activeElement === focusBack
      && HOST_CALLS.filter(m=>m.op.startsWith("vault_")).length === vaultBefore, "");
    const confirmation = vaultSecret("Test confirmation", {confirmation:true});
    enterSecret(secretInput, "short"); enterSecret(secretConfirm, "short"); secretForm.requestSubmit(secretSubmit);
    check("short new passwords stay in the dialog", secretDialog.open && !secretInput.checkValidity(), secretDialog.open);
    enterSecret(secretInput, "new-test-password"); enterSecret(secretConfirm, "different-password"); secretForm.requestSubmit(secretSubmit);
    check("new passwords must match and both fields are masked", secretDialog.open && !secretConfirm.checkValidity() && secretConfirm.type === "password", secretDialog.open);
    enterSecret(secretConfirm, "new-test-password"); secretForm.requestSubmit(secretSubmit); await sleep(50);
    const confirmedSecret = await confirmation;
    check("confirmed passwords return once and both controls clear", confirmedSecret?.password === "new-test-password" && !secretInput.value && !secretConfirm.value, "");
    const allowHello = vaultSecret("Test Hello", {allowEmpty:true}); secretForm.requestSubmit(secretSubmit); await sleep(50);
    check("Windows Hello can still use an empty password", (await allowHello)?.password === "", "");
    const vaultControls = vaultSection({name:"Test vault", remembered:false, hello:false, running:false});
    document.body.append(vaultControls);
    const passwordButton = [...vaultControls.querySelectorAll("button")].find(b=>b.textContent.startsWith("Change password"));
    HOST_ANSWERS.vault_password = null;
    passwordButton.click(); await sleep(30);
    enterSecret(secretInput, "oud1-test-recovery-key"); secretForm.requestSubmit(secretSubmit); await sleep(30);
    enterSecret(secretInput, "new-test-password"); enterSecret(secretConfirm, "new-test-password"); secretForm.requestSubmit(secretSubmit); await sleep(50);
    const passwordChange = HOST_CALLS.filter(m=>m.op === "vault_password").pop();
    check("password change preserves recovery-key routing and sends only after confirmation", passwordChange?.args.recovery === "oud1-test-recovery-key"
      && passwordChange.args.password === "" && passwordChange.args.new === "new-test-password", "");
    passwordButton.click(); await sleep(30); secretDialog.querySelector("button[value=cancel]").click(); await sleep(30);
    check("Cancel ignores validation and sends no password change", !secretDialog.open && HOST_CALLS.filter(m=>m.op === "vault_password").length === 1, "");
    HOST_ANSWERS.vault_unlock = null;
    const acceptedUnlock = unlockVault({name:"Test vault"});
    enterSecret(secretInput, "test-only-secret");
    secretForm.requestSubmit(secretSubmit); await sleep(50); await acceptedUnlock; await sleep(30);
    const unlockCall = HOST_CALLS.filter(m=>m.op === "vault_unlock").pop();
    check("unlock sends no persistent-save request and clears its field", unlockCall && !Object.hasOwn(unlockCall.args, "remember")
      && unlockCall.args.password === "test-only-secret" && !secretInput.value, "");
    const legacyUnlock = unlockVault({name:"Test vault", remembered:true});
    check("an old remember preference never skips password entry", secretDialog.open && !secretInput.value, secretDialog.open);
    secretDialog.requestClose(); await sleep(30); await legacyUnlock;
    await unlockVault({name:"Test vault", hello:true});
    check("Windows Hello unlock bypasses password entry", !secretDialog.open && HOST_CALLS.filter(m=>m.op === "vault_unlock").pop()?.args.hello === true, "");
    vaultControls.remove(); focusBack.remove();
    for (const [id, prefix] of [["dlg-vault-new", "vc"], ["dlg-vault-add", "va"]]) {
      const d = document.getElementById(id), pw = document.getElementById(prefix + "-pw");
      check(prefix + " explains the OneDrive path and why the local folder is empty", document.getElementById(prefix + "-remote").labels.length === 1
        && document.getElementById(prefix + "-remote-help").textContent.includes("/Private")
        && document.getElementById(prefix + "-local").labels[0].textContent === "Decrypted folder on this PC"
        && document.getElementById(prefix + "-local-help").textContent.includes("existing files aren't added"), "");
      check(prefix + " has no persistent password option", !document.getElementById(prefix + "-remember"), "");
      pw.value = "unsaved-test-password"; d.showModal();
      check(prefix + " Enter submits the vault action", d.querySelector(":default") === document.getElementById(prefix + "-add"), "");
      document.getElementById(prefix + "-cancel").click(); await sleep(30);
      check(prefix + " Cancel clears its password without validation", !d.open && !pw.value, pw.value);
    }
    showRecovery("old-test-key");
    document.querySelector("#dlg-recovery").close();
    showRecovery("current-test-key");
    document.querySelector("#dlg-recovery").dispatchEvent(new Event("close"));
    await sleep(100);
    check("a delayed close event cannot clear a newly shown recovery key", document.querySelector("#recovery-text").textContent === "current-test-key",
      document.querySelector("#recovery-text").textContent);
    document.querySelector("#recovery-done").click();
    check("saving the recovery key clears it immediately", !document.querySelector("#dlg-recovery").open
      && !document.querySelector("#recovery-text").textContent, document.querySelector("#recovery-text").textContent);
    // A successful pairing must be visible before unrelated settings finish loading.
    const savedVaultSnapshot = HOST_ANSWERS.snapshot, savedRoots = HOST_ANSWERS.roots;
    for (const [op, prefix, dialogId, name, startError] of [
      ["vault_create", "vc", "dlg-vault-new", "New vault", "test agent unavailable"],
      ["vault_add", "va", "dlg-vault-add", "Existing vault", "test agent unavailable"],
      ["vault_create", "vc", "dlg-vault-new", "Running new vault", null],
      ["vault_add", "va", "dlg-vault-add", "Running existing vault", null]
    ]) {
      let previousRoots;
      HOST_ANSWERS.roots = () => new Promise(resolve => previousRoots = resolve);
      const previousRefresh = openFiles();
      await sleep(30);
      HOST_ANSWERS.roots = savedRoots;
      const settingsReplies = [];
      HOST_ANSWERS.snapshot = a => a?.root ? savedVaultSnapshot(a) : new Promise(resolve => settingsReplies.push(resolve));
      HOST_ANSWERS[op] = () => {
        HOST_ANSWERS.roots = [...savedRoots, {id:"vault:" + name, kind:"vault", name,
          local:"C:\\Vaults\\" + name, remote:"/" + name, on_demand:true, running:!startError}];
        return {name, recovery:"OUD1-test-recovery", start_error:startError};
      };
      const dialog = document.getElementById(dialogId);
      document.getElementById(prefix + "-remote").value = "/" + name;
      document.getElementById(prefix + "-local").value = "C:\\Vaults\\" + name;
      document.getElementById(prefix + "-pw").value = "test-only-password";
      if (prefix === "vc") document.querySelector("#vc-pw2").value = "test-only-password";
      dialog.showModal();
      document.getElementById(prefix + "-add").click();
      await sleep(300);
      const registration = HOST_CALLS.filter(m => m.op === op).pop();
      check(op + " never requests persistent password storage", registration && !Object.hasOwn(registration.args, "remember"), "");
      const selected = document.querySelector("#roots [aria-selected=true]");
      check(op + " immediately lists and selects the registered vault", selected?.textContent.includes(name)
        && selected.textContent.includes("locked") === !!startError, selected?.textContent);
      check(op + " reports whether syncing started", document.querySelector("#toast").textContent.includes(startError || "and syncing")
        && !dialog.open && !document.getElementById(prefix + "-add").disabled, document.getElementById(prefix + "-busy").textContent);
      check(op + " clears its password after registration", !document.getElementById(prefix + "-pw").value
        && (prefix !== "vc" || !document.querySelector("#vc-pw2").value), "");
      if (prefix === "vc") {
        check("creation shows the recovery key without waiting for settings", document.querySelector("#dlg-recovery").open
          && document.querySelector("#recovery-text").textContent === "OUD1-test-recovery", document.querySelector("#recovery-text").textContent);
        check("startup errors are visible alongside the recovery key", document.querySelector("#recovery-error").hidden === !startError
          && (!startError || document.querySelector("#recovery-error").textContent.includes(startError)), document.querySelector("#recovery-error").textContent);
      }
      HOST_ANSWERS.snapshot = savedVaultSnapshot;
      for (const resolve of settingsReplies) resolve(savedVaultSnapshot({}));
      await sleep(100);
      previousRoots(savedRoots);
      await previousRefresh;
      check(op + " keeps its tab when an older list reply arrives", document.querySelector("#roots [aria-selected=true]")?.textContent.includes(name),
        document.querySelector("#roots").textContent);
      if (document.querySelector("#dlg-recovery").open) document.querySelector("#dlg-recovery").close();
    }
    HOST_ANSWERS.roots = savedRoots;
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
