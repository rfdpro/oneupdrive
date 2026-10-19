//! What the `oneup` command-line tool and the `oneupd` background agent
//! share: opening a data folder, the test-account guard, sync rounds, the
//! version store's settings, and the agent's control state. Two executables
//! rather than one because Windows ignores changes the Cloud Files
//! provider's own executable makes (`data-safety.md` invariant 12): user-facing
//! tools must stay separate from the agent.

pub mod accounts;
#[cfg(windows)]
pub mod appleftovers;
pub mod apps;
pub mod autopause;
pub mod backoff;
pub mod busy;
pub mod carryover;
pub mod config;
pub mod control;
#[cfg(windows)]
pub mod datafolders;
pub mod filetree;
pub mod flow;
#[cfg(windows)]
pub mod identity;
#[cfg(windows)]
pub mod install;
#[cfg(windows)]
pub mod item;
pub mod launch;
pub mod moveover;
#[cfg(windows)]
pub mod officeint;
pub mod officeopen;
pub mod ondemand;
pub mod ownfile;
pub mod pairs;
pub mod priority;
pub mod problem;
pub mod prune;
#[cfg(windows)]
pub mod recyclebin;
pub mod resolve;
pub mod rulefiles;
pub mod safetywindow;
pub mod settingsapi;
pub mod setup;
#[cfg(windows)]
pub mod shellstatus;
pub mod shortcuts;
pub mod space;
pub mod stoplist;
pub mod sync;
#[cfg(windows)]
pub mod syncroot;
pub mod test_account;
#[cfg(windows)]
pub mod tray;
pub mod unsynced;
pub mod updates;
pub mod vaultpair;
pub mod versions;
