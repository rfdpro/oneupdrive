//! Changing how an item is shared, for the gate's
//! [`oneup_gate::sharing::PermissionChange`] token: removing a permission
//! (a sharing link, or people's access) or setting its expiry. Never the
//! item's content.
//!
//! Immediately before the request, the item's eTag and the permission are
//! read again and must still be what the gate approved (its code);
//! otherwise nothing is sent ([`Error::Changed`], [`Error::Gone`]). The
//! request itself carries `If-Match` on the item's eTag, which Graph
//! documents for both removing and changing a permission (412 otherwise).
//! Afterwards the result is checked: a removed permission must be gone, and
//! an expiry is what OneDrive reports back (Graph documents only `roles`
//! as changeable, so OneDrive may ignore the date: [`Error::NotApplied`]).

use std::future::Future;

use oneup_gate::sharing::{Change, Grant, PermissionChange};
use oneup_graph::{ItemSharing, Permission};

/// The Graph requests sharing needs. Implemented by [`oneup_graph::Graph`]
/// and by [`fake::FakeSharing`] in tests.
pub trait SharingApi: Send + Sync {
    /// Each item's eTag and permissions, in the order given (`None`: gone).
    fn sharing(
        &self,
        ids: &[&str],
    ) -> impl Future<Output = oneup_graph::Result<Vec<oneup_graph::Result<Option<ItemSharing>>>>> + Send;
    /// Removes a permission, if the item's eTag is still `etag`.
    fn delete_permission(
        &self,
        item: &str,
        perm: &str,
        etag: &str,
    ) -> impl Future<Output = oneup_graph::Result<()>> + Send;
    /// Sets a permission's expiry, if the item's eTag is still `etag`;
    /// returns the permission as OneDrive then has it.
    fn set_permission_expiry(
        &self,
        item: &str,
        perm: &str,
        etag: &str,
        expires_ms: i64,
    ) -> impl Future<Output = oneup_graph::Result<Permission>> + Send;
}

impl SharingApi for oneup_graph::Graph {
    async fn sharing(&self, ids: &[&str]) -> oneup_graph::Result<Vec<oneup_graph::Result<Option<ItemSharing>>>> {
        oneup_graph::Graph::sharing(self, ids).await
    }
    async fn delete_permission(&self, item: &str, perm: &str, etag: &str) -> oneup_graph::Result<()> {
        oneup_graph::Graph::delete_permission(self, item, perm, etag).await
    }
    async fn set_permission_expiry(
        &self,
        item: &str,
        perm: &str,
        etag: &str,
        expires_ms: i64,
    ) -> oneup_graph::Result<Permission> {
        oneup_graph::Graph::set_permission_expiry(self, item, perm, etag, expires_ms).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The item or the permission changed since the gate saw it. Nothing
    /// was changed.
    #[error("it changed in OneDrive meanwhile; look again")]
    Changed,
    /// The item or the permission no longer exists. Nothing was changed.
    #[error("the item or the permission is gone from OneDrive")]
    Gone,
    /// OneDrive answered, but the permission isn't as asked.
    #[error("OneDrive didn't make the change")]
    NotApplied,
    /// The request was sent, and whether it took effect couldn't be read
    /// back.
    #[error("OneDrive didn't confirm the change; look again")]
    Unconfirmed,
    #[error(transparent)]
    Graph(#[from] oneup_graph::Error),
}

impl Error {
    /// What went wrong, safe to keep (the activity log): never a response
    /// body, which can name people or hold a sharing link.
    pub fn summary(&self) -> String {
        match self {
            Error::Graph(e) => e.summary(),
            other => other.to_string(),
        }
    }
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Stopped,
    /// The expiry OneDrive has now: the one asked for.
    Expires(i64),
}

/// How close an expiry OneDrive reports must be to the one asked for:
/// Graph's times have whole seconds.
const SAME_TIME_MS: i64 = 1000;

/// The gate's view of permission `p` of an item, as read (`s`): the one
/// place a Graph permission becomes a [`Grant`], so a listing's codes and
/// the checks before acting agree.
pub fn grant(item: &str, s: &ItemSharing, p: &Permission) -> Grant {
    let mut roles = p.roles.clone();
    roles.sort();
    let mut who = p.granted_to.clone();
    who.sort();
    let o = |v: &Option<String>| v.clone().unwrap_or_default();
    Grant {
        item: item.to_string(),
        etag: s.etag.clone(),
        permission: p.id.clone(),
        inherited: p.inherited_from.is_some(),
        owner: p.is_owner(),
        root: s.root,
        describe: format!(
            "roles={}\ntype={}\nscope={}\nurl={}\nexpires={}\npassword={}\nwho={}\nfrom={}",
            roles.join(","),
            o(&p.link_type),
            o(&p.link_scope),
            o(&p.link_url),
            p.expires_ms.map(|t| t.to_string()).unwrap_or_default(),
            p.has_password,
            who.join("\n"),
            o(&p.inherited_from),
        ),
    }
}

/// The item's sharing as OneDrive has it now (`None`: the item is gone).
async fn item_sharing<A: SharingApi>(api: &A, item: &str) -> oneup_graph::Result<Option<ItemSharing>> {
    let mut read = api.sharing(&[item]).await?;
    if read.is_empty() { Ok(None) } else { read.swap_remove(0) }
}

/// One permission as OneDrive has it now ([`seen`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// The item is gone, with its permissions: that says nothing about
    /// whether a removal asked for took effect before.
    ItemGone,
    /// The item is there, without the permission.
    PermissionGone,
    /// It's there, with this expiry.
    Present { expires_ms: Option<i64> },
}

/// Permission `perm` of item `item` as OneDrive has it now.
pub async fn seen<A: SharingApi>(api: &A, item: &str, perm: &str) -> oneup_graph::Result<Seen> {
    Ok(match item_sharing(api, item).await? {
        None => Seen::ItemGone,
        Some(s) => match s.permissions.iter().find(|p| p.id == perm) {
            None => Seen::PermissionGone,
            Some(p) => Seen::Present { expires_ms: p.expires_ms },
        },
    })
}

/// Whether a change asked of OneDrive took effect, judged only from what
/// OneDrive has now ([`seen`]), never from its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judged {
    Made(Done),
    /// The permission is there, not as asked.
    NotMade,
    /// The item or the permission is gone, so it can't be told (a removal
    /// counts as made only while the item is still there).
    Gone,
}

pub fn judge(change: Change, now: Seen) -> Judged {
    match (change, now) {
        (_, Seen::ItemGone) | (Change::Expire(_), Seen::PermissionGone) => Judged::Gone,
        (Change::Stop, Seen::PermissionGone) => Judged::Made(Done::Stopped),
        (Change::Stop, Seen::Present { .. }) => Judged::NotMade,
        (Change::Expire(at), Seen::Present { expires_ms: Some(e) }) if (e - at).abs() < SAME_TIME_MS => {
            Judged::Made(Done::Expires(at))
        }
        (Change::Expire(_), Seen::Present { .. }) => Judged::NotMade,
    }
}

/// Permission `perm` of item `item` as OneDrive has it now (`None`: the
/// item or the permission is gone).
pub async fn current<A: SharingApi>(api: &A, item: &str, perm: &str) -> Result<Option<Grant>, Error> {
    let Some(s) = item_sharing(api, item).await? else { return Ok(None) };
    Ok(s.permissions.iter().find(|p| p.id == perm).map(|p| grant(item, &s, p)))
}

/// Makes the change the token approved, if the item and the permission are
/// still as approved (see the module docs). The token is used up.
///
/// Whatever OneDrive answers, the permission is read back and the outcome
/// is what OneDrive has then ([`judge`]): a success whose answer was lost
/// and the request sent again comes back 404 or 412, though it took effect,
/// and an answer can claim a change OneDrive didn't make.
pub async fn apply<A: SharingApi>(api: &A, t: PermissionChange) -> Result<Done, Error> {
    let now = current(api, t.item(), t.permission()).await?.ok_or(Error::Gone)?;
    if now.code() != t.code() {
        return Err(Error::Changed);
    }
    let sent = match t.change() {
        Change::Stop => api.delete_permission(t.item(), t.permission(), t.etag()).await,
        Change::Expire(at) => api.set_permission_expiry(t.item(), t.permission(), t.etag(), at).await.map(|_| ()),
    };
    // A refusal other than "gone" or "changed" is definite: nothing was done.
    if let Err(e) = &sent
        && matches!(e.status(), Some(s) if (400..500).contains(&s) && s != 404 && s != 412)
    {
        return Err(sent.err().map_or(Error::NotApplied, Error::Graph));
    }
    let Ok(after) = seen(api, t.item(), t.permission()).await else { return Err(Error::Unconfirmed) };
    match judge(t.change(), after) {
        Judged::Made(done) => Ok(done),
        Judged::Gone => Err(Error::Gone),
        // Still there, not as asked: a 412 is the item having changed; any
        // other refusal is that refusal (a 404 here isn't "gone": the item
        // and the permission are both there); no refusal at all means
        // OneDrive ignored the request.
        Judged::NotMade => Err(match sent.err() {
            Some(e) if e.status() == Some(412) => Error::Changed,
            Some(e) => Error::Graph(e),
            None => Error::NotApplied,
        }),
    }
}

/// An in-memory OneDrive's sharing, for tests here and in the binaries.
#[cfg(any(test, feature = "fake"))]
pub mod fake {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use oneup_graph::{ItemSharing, Permission};

    /// One item's sharing.
    #[derive(Debug, Clone)]
    pub struct Item {
        pub etag: u64,
        pub root: bool,
        pub permissions: Vec<Permission>,
    }

    type Hook = Box<dyn FnOnce(&mut BTreeMap<String, Item>) + Send>;

    #[derive(Default)]
    pub struct FakeSharing {
        pub items: Mutex<BTreeMap<String, Item>>,
        /// Requests that changed something, in order (`delete item perm`,
        /// `expire item perm ms`), and refused ones (`refused …`).
        pub log: Mutex<Vec<String>>,
        /// Run just before the next change request is judged: something
        /// changing in OneDrive between the last check and the request.
        pub before_write: Mutex<Option<Hook>>,
        /// Answer a change of expiry without making it (OneDrive may).
        pub ignore_expiry: bool,
        /// Answer a removal without making it.
        pub ignore_delete: bool,
        /// Make the change, then answer as the request sent again after a
        /// lost success would be answered: 412.
        pub lose_answer: bool,
        /// Refuse every change with this status and body.
        pub refuse: Option<(u16, String)>,
        /// After a change is asked for, every read fails (the network gone).
        pub fail_reads_after_write: bool,
        /// Whether a change has been asked for (set by the fake).
        pub wrote: std::sync::atomic::AtomicBool,
    }

    impl FakeSharing {
        /// Adds an item with these permissions (eTag 1).
        pub fn add(&self, id: &str, permissions: Vec<Permission>) {
            let item = Item { etag: 1, root: false, permissions };
            self.items.lock().unwrap_or_else(|p| p.into_inner()).insert(id.to_string(), item);
        }

        pub fn etag_of(n: u64) -> String {
            format!("\"{{E}},{n}\"")
        }

        pub fn log(&self) -> Vec<String> {
            self.log.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }

        fn write<T>(
            &self,
            item: &str,
            etag: &str,
            f: impl FnOnce(&mut Item) -> oneup_graph::Result<T>,
            what: String,
        ) -> oneup_graph::Result<T> {
            let mut items = self.items.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = self.before_write.lock().unwrap_or_else(|p| p.into_inner()).take() {
                h(&mut items);
            }
            self.wrote.store(true, std::sync::atomic::Ordering::SeqCst);
            let mut log = self.log.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((status, body)) = &self.refuse {
                log.push(format!("refused {what}"));
                return Err(oneup_graph::Error::refused(*status, "fake", body.clone()));
            }
            let Some(it) = items.get_mut(item) else {
                log.push(format!("refused {what}"));
                return Err(oneup_graph::Error::refused(404, "fake", String::new()));
            };
            if Self::etag_of(it.etag) != etag {
                log.push(format!("refused {what}"));
                return Err(oneup_graph::Error::refused(
                    412,
                    "fake",
                    r#"{"error":{"code":"resourceModified"}}"#.into(),
                ));
            }
            let out = f(it)?;
            log.push(what);
            if self.lose_answer {
                return Err(oneup_graph::Error::refused(412, "fake", String::new()));
            }
            Ok(out)
        }

        pub fn permission(id: &str, link: Option<(&str, &str)>, roles: &[&str]) -> Permission {
            Permission {
                id: id.into(),
                roles: roles.iter().map(|r| r.to_string()).collect(),
                link_type: link.map(|l| l.0.to_string()),
                link_scope: link.map(|l| l.1.to_string()),
                link_url: link.map(|_| format!("https://1drv.ms/{id}")),
                granted_to: Vec::new(),
                expires_ms: None,
                inherited_from: None,
                has_password: false,
            }
        }
    }

    fn missing() -> oneup_graph::Error {
        oneup_graph::Error::refused(404, "fake", String::new())
    }

    impl super::SharingApi for FakeSharing {
        async fn sharing(&self, ids: &[&str]) -> oneup_graph::Result<Vec<oneup_graph::Result<Option<ItemSharing>>>> {
            if self.fail_reads_after_write && self.wrote.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(oneup_graph::Error::Exhausted(8));
            }
            let items = self.items.lock().unwrap_or_else(|p| p.into_inner());
            Ok(ids
                .iter()
                .map(|id| {
                    Ok(items.get(*id).map(|it| ItemSharing {
                        etag: Self::etag_of(it.etag),
                        root: it.root,
                        permissions: it.permissions.clone(),
                    }))
                })
                .collect())
        }

        async fn delete_permission(&self, item: &str, perm: &str, etag: &str) -> oneup_graph::Result<()> {
            let ignore = self.ignore_delete;
            self.write(
                item,
                etag,
                |it| {
                    let at = it.permissions.iter().position(|p| p.id == perm).ok_or_else(missing)?;
                    if !ignore {
                        it.permissions.remove(at);
                        it.etag += 1;
                    }
                    Ok(())
                },
                format!("delete {item} {perm}"),
            )
        }

        async fn set_permission_expiry(
            &self,
            item: &str,
            perm: &str,
            etag: &str,
            expires_ms: i64,
        ) -> oneup_graph::Result<Permission> {
            let ignore = self.ignore_expiry;
            self.write(
                item,
                etag,
                |it| {
                    let p = it.permissions.iter_mut().find(|p| p.id == perm).ok_or_else(missing)?;
                    if !ignore {
                        p.expires_ms = Some(expires_ms);
                    }
                    Ok(p.clone())
                },
                format!("expire {item} {perm} {expires_ms}"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::fake::FakeSharing;
    use super::*;
    use oneup_gate::sharing::{Refusal, Request, review_permission};

    fn drive() -> FakeSharing {
        let f = FakeSharing::default();
        let mut people = FakeSharing::permission("people", None, &["read"]);
        people.granted_to = vec!["Sam".into()];
        let mut inherited = FakeSharing::permission("inh", Some(("view", "anonymous")), &["read"]);
        inherited.inherited_from = Some("F!1".into());
        f.add(
            "I!1",
            vec![
                FakeSharing::permission("owner", None, &["owner"]),
                FakeSharing::permission("link", Some(("edit", "anonymous")), &["write"]),
                people,
                inherited,
            ],
        );
        f
    }

    fn block_on<T>(f: impl Future<Output = T>) -> T {
        // No runtime needed: the fake never waits.
        let mut f = std::pin::pin!(f);
        let waker = std::task::Waker::noop();
        match f.as_mut().poll(&mut std::task::Context::from_waker(waker)) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("the fake never waits"),
        }
    }

    /// What a listing showed, and the token for acting on it now.
    fn approve(f: &FakeSharing, seen: &Grant, change: Change) -> Result<PermissionChange, Refusal> {
        let now = block_on(current(f, &seen.item, &seen.permission)).unwrap();
        let req = Request { seen: seen.code(), change, confirmed: true, experimental: true, now_ms: 0 };
        review_permission(&req, now.as_ref())
    }

    fn listed(f: &FakeSharing, perm: &str) -> Grant {
        block_on(current(f, "I!1", perm)).unwrap().unwrap()
    }

    #[test]
    fn stopping_a_link_removes_it_and_nothing_else() {
        let f = drive();
        let t = approve(&f, &listed(&f, "link"), Change::Stop).unwrap();
        assert_eq!(block_on(apply(&f, t)).unwrap(), Done::Stopped);
        let left: Vec<String> = f.items.lock().unwrap()["I!1"].permissions.iter().map(|p| p.id.clone()).collect();
        assert_eq!(left, ["owner", "people", "inh"]);
        assert_eq!(f.log(), ["delete I!1 link"]);
    }

    #[test]
    fn a_change_since_the_listing_sends_nothing() {
        // The link replaced after it was listed: refused by the gate.
        let f = drive();
        let seen = listed(&f, "link");
        f.items.lock().unwrap().get_mut("I!1").unwrap().permissions[1].link_type = Some("view".into());
        assert_eq!(approve(&f, &seen, Change::Stop).unwrap_err(), Refusal::Changed);

        // Changed between the gate and the effect: the effect re-checks.
        let f = drive();
        let t = approve(&f, &listed(&f, "link"), Change::Stop).unwrap();
        f.items.lock().unwrap().get_mut("I!1").unwrap().etag = 7;
        assert!(matches!(block_on(apply(&f, t)), Err(Error::Changed)));
        let f2 = drive();
        let t2 = approve(&f2, &listed(&f2, "link"), Change::Stop).unwrap();
        f2.items.lock().unwrap().clear();
        assert!(matches!(block_on(apply(&f2, t2)), Err(Error::Gone)));
        assert!(f.log().is_empty() && f2.log().is_empty(), "no request was sent");
    }

    #[test]
    fn an_edit_landing_just_before_the_request_is_refused_by_if_match() {
        let f = drive();
        let t = approve(&f, &listed(&f, "link"), Change::Stop).unwrap();
        *f.before_write.lock().unwrap() = Some(Box::new(|items| items.get_mut("I!1").unwrap().etag += 1));
        assert!(matches!(block_on(apply(&f, t)), Err(Error::Changed)));
        assert_eq!(f.log(), ["refused delete I!1 link"]);
        assert_eq!(f.items.lock().unwrap()["I!1"].permissions.len(), 4, "still shared");
    }

    #[test]
    fn inherited_and_owner_permissions_get_no_token() {
        let f = drive();
        assert_eq!(approve(&f, &listed(&f, "inh"), Change::Stop).unwrap_err(), Refusal::Inherited);
        assert_eq!(approve(&f, &listed(&f, "owner"), Change::Stop).unwrap_err(), Refusal::Owner);
    }

    #[test]
    fn a_removal_or_expiry_onedrive_ignored_is_reported() {
        let f = FakeSharing { ignore_delete: true, ..drive() };
        let t = approve(&f, &listed(&f, "link"), Change::Stop).unwrap();
        assert!(matches!(block_on(apply(&f, t)), Err(Error::NotApplied)));

        let f = FakeSharing { ignore_expiry: true, ..drive() };
        let t = approve(&f, &listed(&f, "people"), Change::Expire(9_000)).unwrap();
        assert!(matches!(block_on(apply(&f, t)), Err(Error::NotApplied)));

        // Ignored while another expiry was set already: still not done.
        let f = FakeSharing { ignore_expiry: true, ..drive() };
        f.items.lock().unwrap().get_mut("I!1").unwrap().permissions[2].expires_ms = Some(50_000);
        let t = approve(&f, &listed(&f, "people"), Change::Expire(9_000)).unwrap();
        assert!(matches!(block_on(apply(&f, t)), Err(Error::NotApplied)));

        let f = drive();
        let t = approve(&f, &listed(&f, "people"), Change::Expire(9_000)).unwrap();
        assert_eq!(block_on(apply(&f, t)).unwrap(), Done::Expires(9_000));
        assert_eq!(f.log(), ["expire I!1 people 9000"]);
    }

    /// A success whose answer was lost, and the request sent again, comes
    /// back 412: what OneDrive has then decides.
    #[test]
    fn a_lost_answer_is_settled_by_reading_back() {
        let f = FakeSharing { lose_answer: true, ..drive() };
        let t = approve(&f, &listed(&f, "link"), Change::Stop).unwrap();
        assert_eq!(block_on(apply(&f, t)).unwrap(), Done::Stopped);
        let f = FakeSharing { lose_answer: true, ..drive() };
        let t = approve(&f, &listed(&f, "people"), Change::Expire(9_000)).unwrap();
        assert_eq!(block_on(apply(&f, t)).unwrap(), Done::Expires(9_000));
        // Refused for real: still there, so it's the refusal.
        let f = drive();
        let t = approve(&f, &listed(&f, "link"), Change::Stop).unwrap();
        *f.before_write.lock().unwrap() = Some(Box::new(|items| items.get_mut("I!1").unwrap().etag += 1));
        assert!(matches!(block_on(apply(&f, t)), Err(Error::Changed)));
    }

    /// The item deleted just before the request: 404, and nothing to read
    /// back. That isn't a removal made.
    #[test]
    fn an_item_deleted_meanwhile_is_not_a_removal_made() {
        let f = drive();
        let t = approve(&f, &listed(&f, "link"), Change::Stop).unwrap();
        *f.before_write.lock().unwrap() = Some(Box::new(|items| {
            items.remove("I!1");
        }));
        assert!(matches!(block_on(apply(&f, t)), Err(Error::Gone)));
        // The judgement itself: only the permission gone, item there, is made.
        assert_eq!(judge(Change::Stop, Seen::ItemGone), Judged::Gone);
        assert_eq!(judge(Change::Stop, Seen::PermissionGone), Judged::Made(Done::Stopped));
        assert_eq!(judge(Change::Expire(5_000), Seen::PermissionGone), Judged::Gone);
        assert_eq!(
            judge(Change::Expire(5_000), Seen::Present { expires_ms: Some(5_400) }),
            Judged::Made(Done::Expires(5_000))
        );
        assert_eq!(judge(Change::Expire(5_000), Seen::Present { expires_ms: Some(6_000) }), Judged::NotMade);
        assert_eq!(judge(Change::Expire(5_000), Seen::Present { expires_ms: None }), Judged::NotMade);
    }

    /// A 404 while the item and the permission are both still there isn't
    /// "gone": it's OneDrive refusing, and nothing changed.
    #[test]
    fn a_not_found_with_the_permission_still_there_is_a_failure() {
        let body = r#"{"error":{"code":"itemNotFound"}}"#;
        let f = FakeSharing { refuse: Some((404, body.into())), ..drive() };
        let t = approve(&f, &listed(&f, "link"), Change::Stop).unwrap();
        let e = block_on(apply(&f, t)).unwrap_err();
        assert!(matches!(&e, Error::Graph(g) if g.status() == Some(404)), "{e:?}");
        assert_eq!(e.summary(), "OneDrive refused it (404, itemNotFound)");
        let f = FakeSharing { refuse: Some((404, body.into())), ..drive() };
        let t = approve(&f, &listed(&f, "people"), Change::Expire(9_000)).unwrap();
        let e = block_on(apply(&f, t)).unwrap_err();
        assert!(matches!(&e, Error::Graph(g) if g.status() == Some(404)), "{e:?}");
        assert_eq!(f.items.lock().unwrap()["I!1"].permissions.len(), 4, "nothing changed");
    }

    #[test]
    fn a_refusal_is_summed_up_without_its_body() {
        let body = r#"{"error":{"code":"accessDenied","message":"sam@example.com, https://1drv.ms/x?authkey=!K"}}"#;
        let f = FakeSharing { refuse: Some((403, body.into())), ..drive() };
        let t = approve(&f, &listed(&f, "link"), Change::Stop).unwrap();
        let e = block_on(apply(&f, t)).unwrap_err();
        assert_eq!(e.summary(), "OneDrive refused it (403, accessDenied)");
    }

    #[test]
    fn codes_follow_what_is_shown() {
        let f = drive();
        let a = listed(&f, "people");
        f.items.lock().unwrap().get_mut("I!1").unwrap().permissions[2].granted_to.push("Ana".into());
        assert_ne!(a.code(), listed(&f, "people").code(), "someone added");
        let b = listed(&f, "people");
        f.items.lock().unwrap().get_mut("I!1").unwrap().permissions[2].expires_ms = Some(5);
        assert_ne!(b.code(), listed(&f, "people").code(), "an expiry set");
    }
}
