//! Test-only (feature `test-helpers`): a program of its own that writes a
//! SQLite database, for `tests/sync.rs`. A different executable from the
//! test, as a user's program would be (AGENTS.md). Never built into a
//! release.
//!
//! * `write DB SECONDS [wal|delete] [STOPS]`: writes transactions without pause
//!   for `SECONDS`, each adding a row and raising a counter in the same
//!   transaction (so a consistent copy has as many rows as the counter
//!   says). In WAL mode the `-wal` is truncated now and then (`PRAGMA
//!   wal_checkpoint(TRUNCATE)`) and the database closed and opened again,
//!   so there are moments when a copy may be sent; SQLite's own
//!   checkpoints (every 20 pages) write the file while the `-wal` still
//!   holds changes. At its first `STOPS` quiet moments it prints `quiet`
//!   and waits for a line on its input. Prints `done N`.
//! * `hold DB ROWS`: adds `ROWS` rows in WAL mode without a checkpoint,
//!   prints `held`, keeps the database open until a line arrives on its
//!   input, then closes it (which checkpoints and removes the `-wal`) and
//!   prints `closed`. Killed instead, it leaves the `-wal` behind.
//! * `keep DB [MODE]`: opens the database (made if new, in `MODE`,
//!   `delete` by default) without changing it, prints `open`, and keeps it
//!   open until a line arrives on its input, then prints `closed`.

use std::io::BufRead as _;
use std::time::{Duration, Instant};

use rusqlite::Connection;

fn open(path: &str, mode: &str) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(10))?;
    c.query_row(&format!("PRAGMA journal_mode = {mode}"), [], |_| Ok(()))?;
    c.execute_batch(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS item (id INTEGER PRIMARY KEY, body TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS counter (n INTEGER NOT NULL);
         INSERT INTO counter SELECT 0 WHERE NOT EXISTS (SELECT 1 FROM counter);
         COMMIT;",
    )?;
    Ok(c)
}

fn add(c: &mut Connection, i: u64) -> rusqlite::Result<()> {
    let tx = c.transaction()?;
    // Rows of some size, so the database grows over many pages.
    tx.execute("INSERT INTO item (body) VALUES (?1)", [format!("{i:08}").repeat(40)])?;
    tx.execute("UPDATE counter SET n = n + 1", [])?;
    tx.commit()
}

fn main() -> rusqlite::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("write") => {
            let (path, secs) = (&args[2], args[3].parse::<u64>().unwrap_or(5));
            let mode = args.get(4).map_or("wal", String::as_str);
            // The first quiet moments wait for the test to say go, so it can
            // run a round at each, whatever the machine's load.
            let mut stops = args.get(5).and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
            let end = Instant::now() + Duration::from_secs(secs);
            // Checkpoints of its own as well (into the file, the -wal left
            // as it is), so the file changes while the -wal holds more.
            let open = |path: &str| -> rusqlite::Result<Connection> {
                let c = open(path, mode)?;
                c.query_row("PRAGMA wal_autocheckpoint = 20", [], |_| Ok(()))?;
                Ok(c)
            };
            let mut c = open(path)?;
            let mut i = 0u64;
            // The stops come first however slow the machine: under load the
            // time can run out before the 50th transaction.
            while stops > 0 || Instant::now() < end {
                add(&mut c, i)?;
                i += 1;
                if i.is_multiple_of(50) {
                    if mode == "wal" {
                        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
                    }
                    // A moment with nothing outside the database.
                    if stops > 0 {
                        stops -= 1;
                        println!("quiet");
                        let mut line = String::new();
                        let _ = std::io::stdin().lock().read_line(&mut line);
                    } else {
                        std::thread::sleep(Duration::from_millis(150));
                    }
                }
                if i.is_multiple_of(400) {
                    drop(c);
                    std::thread::sleep(Duration::from_millis(150));
                    c = open(path)?;
                }
            }
            println!("done {i}");
        }
        Some("hold") => {
            let (path, rows) = (&args[2], args[3].parse::<u64>().unwrap_or(10));
            let mut c = open(path, "wal")?;
            c.query_row("PRAGMA wal_autocheckpoint = 0", [], |_| Ok(()))?;
            for i in 0..rows {
                add(&mut c, i)?;
            }
            println!("held");
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            drop(c);
            println!("closed");
        }
        Some("keep") => {
            let mode = args.get(3).map_or("delete", String::as_str);
            let c = open(&args[2], mode)?;
            println!("open");
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            drop(c);
            println!("closed");
        }
        _ => eprintln!("usage: oneup-test-sqlite write DB SECONDS [wal|delete] | hold DB ROWS | keep DB [MODE]"),
    }
    Ok(())
}
