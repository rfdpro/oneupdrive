//! Changing how an item is shared: removing a sharing link or people's
//! access (**Stop sharing**), or setting when it stops working (**Set
//! expiry**). Neither touches the item's content, but both change OneDrive
//! for other people, so they run only with a [`PermissionChange`] token
//! from [`review_permission`], which only this crate can make, through
//! `oneup_effects::sharing`.
//!
//! The user acts on a permission as it was listed. The listing gave it a
//! code ([`Grant::code`]) over the item, its eTag then, and everything shown
//! about the permission; the caller reads both again just before asking,
//! and the gate approves only if the code still matches: an item edited,
//! or a link changed or replaced since, is refused as [`Refusal::Changed`]
//! ("look again"), never acted on. The effects layer reads them a third
//! time immediately before the request and sends it with `If-Match` on the
//! item's eTag (`data-safety.md` invariants 2 and 5).
//!
//! Graph documents only `roles` as changeable on a permission
//! (`permission: update`), so setting an expiry relies on behaviour it
//! doesn't document: it's approved only with experimental options on, and
//! the effects layer checks the date OneDrive reports back.

use crate::Fingerprint;

/// One permission of one item, as OneDrive has it (built by
/// `oneup_effects::sharing::grant`, the one place a Graph permission
/// becomes this).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub item: String,
    /// The item's eTag when read.
    pub etag: String,
    pub permission: String,
    /// It comes from a folder above: only removable there.
    pub inherited: bool,
    /// The owner's own access.
    pub owner: bool,
    /// The item is the drive's root, whose permissions OneDrive Personal
    /// won't change.
    pub root: bool,
    /// Everything else shown about it (kind, roles, scope, expiry, who, in
    /// a canonical form).
    pub describe: String,
}

impl Grant {
    /// The code a listing shows for it, stable across runs: FNV-1a, a
    /// staleness check, not a security measure.
    pub fn code(&self) -> Fingerprint {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let flags = [u8::from(self.inherited), u8::from(self.owner), u8::from(self.root)];
        let parts: [&[u8]; 6] = [
            b"permission",
            self.item.as_bytes(),
            self.etag.as_bytes(),
            self.permission.as_bytes(),
            &flags,
            self.describe.as_bytes(),
        ];
        for part in parts {
            for &b in part.iter().chain(&[0]) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        Fingerprint(h)
    }
}

/// What to do with the permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Remove it: the link stops working, or the people lose access.
    Stop,
    /// It stops working at this time (Unix milliseconds).
    Expire(i64),
}

/// What the user asked for, and what's known around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// The code of the permission as the user saw it.
    pub seen: Fingerprint,
    pub change: Change,
    /// The user confirmed this change.
    pub confirmed: bool,
    /// Experimental options are on (needed for [`Change::Expire`]).
    pub experimental: bool,
    /// Now (Unix milliseconds), for judging an expiry.
    pub now_ms: i64,
}

/// Why the gate won't make the change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the item or the permission is gone from OneDrive")]
    Gone,
    #[error("it changed in OneDrive since it was listed; look again")]
    Changed,
    #[error("it's inherited from a folder above; change it on that folder")]
    Inherited,
    #[error("that's the owner's own access")]
    Owner,
    #[error("OneDrive doesn't change the sharing of the drive's top folder")]
    Root,
    #[error("the expiry date has passed")]
    InThePast,
    #[error(
        "setting an expiry is experimental: turn on experimental options first (`oneup settings --experimental on`)"
    )]
    Experimental,
    #[error("not confirmed")]
    NotConfirmed,
}

/// The gate's approval to change one permission of one item, as it was
/// when reviewed. The effects layer re-checks that the item's eTag and the
/// permission's code are still these just before acting.
#[derive(Debug, PartialEq, Eq)]
pub struct PermissionChange {
    item: String,
    etag: String,
    permission: String,
    code: Fingerprint,
    change: Change,
    _seal: Seal,
}

#[derive(Debug, PartialEq, Eq)]
struct Seal;

impl PermissionChange {
    pub fn item(&self) -> &str {
        &self.item
    }
    /// The item's eTag the change is conditional on (`If-Match`).
    pub fn etag(&self) -> &str {
        &self.etag
    }
    pub fn permission(&self) -> &str {
        &self.permission
    }
    /// The permission's code when reviewed.
    pub fn code(&self) -> Fingerprint {
        self.code
    }
    pub fn change(&self) -> Change {
        self.change
    }
}

/// Approves changing permission `now` (as read just now; `None`: the item
/// or the permission is gone) only if it's still exactly what the user saw,
/// is the item's own (not inherited, not the owner's, not on the drive's
/// root), and the user confirmed.
pub fn review_permission(req: &Request, now: Option<&Grant>) -> Result<PermissionChange, Refusal> {
    let g = now.ok_or(Refusal::Gone)?;
    if g.code() != req.seen {
        return Err(Refusal::Changed);
    }
    if g.root {
        return Err(Refusal::Root);
    }
    if g.owner {
        return Err(Refusal::Owner);
    }
    if g.inherited {
        return Err(Refusal::Inherited);
    }
    if let Change::Expire(at) = req.change {
        if !req.experimental {
            return Err(Refusal::Experimental);
        }
        if at <= req.now_ms {
            return Err(Refusal::InThePast);
        }
    }
    if !req.confirmed {
        return Err(Refusal::NotConfirmed);
    }
    Ok(PermissionChange {
        item: g.item.clone(),
        etag: g.etag.clone(),
        permission: g.permission.clone(),
        code: g.code(),
        change: req.change,
        _seal: Seal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> Grant {
        Grant {
            item: "I!1".into(),
            etag: "\"{E},3\"".into(),
            permission: "p1".into(),
            inherited: false,
            owner: false,
            root: false,
            describe: "link edit anonymous; expires never".into(),
        }
    }

    fn req(seen: &Grant, change: Change) -> Request {
        Request { seen: seen.code(), change, confirmed: true, experimental: false, now_ms: 1_000 }
    }

    #[test]
    fn stops_sharing_only_what_the_user_saw() {
        let g = grant();
        let t = review_permission(&req(&g, Change::Stop), Some(&g)).unwrap();
        assert_eq!((t.item(), t.etag(), t.permission(), t.change()), ("I!1", "\"{E},3\"", "p1", Change::Stop));
        assert_eq!(t.code(), g.code());

        let r = req(&g, Change::Stop);
        let changed = |f: fn(&mut Grant)| {
            let mut now = grant();
            f(&mut now);
            review_permission(&r, Some(&now)).unwrap_err()
        };
        // The item edited, the link changed or replaced since it was listed.
        assert_eq!(changed(|g| g.etag = "\"{E},4\"".into()), Refusal::Changed);
        assert_eq!(changed(|g| g.describe = "link view anonymous; expires never".into()), Refusal::Changed);
        assert_eq!(changed(|g| g.permission = "p2".into()), Refusal::Changed);
        assert_eq!(changed(|g| g.item = "I!2".into()), Refusal::Changed);
        assert_eq!(review_permission(&r, None).unwrap_err(), Refusal::Gone);
        assert_eq!(review_permission(&Request { confirmed: false, ..r }, Some(&g)).unwrap_err(), Refusal::NotConfirmed);
    }

    #[test]
    fn inherited_owner_and_root_permissions_are_never_changed() {
        for (f, why) in [
            ((|g: &mut Grant| g.inherited = true) as fn(&mut Grant), Refusal::Inherited),
            (|g: &mut Grant| g.owner = true, Refusal::Owner),
            (|g: &mut Grant| g.root = true, Refusal::Root),
        ] {
            let mut g = grant();
            f(&mut g);
            // Even when that's exactly what was listed.
            assert_eq!(review_permission(&req(&g, Change::Stop), Some(&g)).unwrap_err(), why);
        }
    }

    #[test]
    fn an_expiry_needs_experimental_options_and_a_future_date() {
        let g = grant();
        let r = req(&g, Change::Expire(5_000));
        assert_eq!(review_permission(&r, Some(&g)).unwrap_err(), Refusal::Experimental);
        let r = Request { experimental: true, ..r };
        assert_eq!(review_permission(&r, Some(&g)).unwrap().change(), Change::Expire(5_000));
        let past = Request { change: Change::Expire(1_000), ..r };
        assert_eq!(review_permission(&past, Some(&g)).unwrap_err(), Refusal::InThePast);
    }

    #[test]
    fn codes_are_stable_and_tell_every_field_apart() {
        let g = grant();
        assert_eq!(g.code(), grant().code());
        let mut moved = grant();
        moved.item = "I!1\"{E},3".into();
        moved.etag = "\"".into();
        assert_ne!(g.code(), moved.code(), "fields are separated, not run together");
        let mut flagged = grant();
        flagged.inherited = true;
        assert_ne!(g.code(), flagged.code());
    }
}
