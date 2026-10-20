//! The subset of Microsoft Graph's `driveItem` that OneUpDrive uses.

use base64::Engine as _;
use serde::Deserialize;

/// Properties requested for every item. Keeping this minimal measurably
/// speeds up enumeration (measured: ~22% lower page latency). `shared` is
/// there for the sharing audit: only shared items carry the facet, so it
/// adds bytes to their entries alone. Microsoft's delta documentation lists
/// no property that OneDrive (consumer) leaves out of created or modified
/// items ("Remarks", `driveItem: delta`), so the facet arrives with delta.
pub const ITEM_SELECT: &str =
    "id,name,size,eTag,parentReference,file,folder,package,deleted,root,fileSystemInfo,shared";

/// The account's drive (`GET /me/drive`), only what identifies it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: String,
    /// `personal` for OneDrive Personal, the only kind supported.
    #[serde(default)]
    pub drive_type: Option<String>,
    /// Whose it is: shown so people can tell which account signed in.
    #[serde(default)]
    pub owner: Option<Owner>,
}

impl Drive {
    /// The owner's display name, if Graph gave one.
    pub fn owner_name(&self) -> Option<&str> {
        self.owner.as_ref()?.user.as_ref()?.display_name.as_deref().filter(|n| !n.is_empty())
    }
}

/// A drive's `owner` (an identity set; only the user part is used).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Owner {
    #[serde(default)]
    pub user: Option<Identity>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    #[serde(default)]
    pub display_name: Option<String>,
}

/// What a download needs to know first (`GET item`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DownloadInfo {
    pub id: String,
    #[serde(default, rename = "eTag")]
    pub e_tag: Option<String>,
    #[serde(default)]
    pub folder: Option<FolderFacet>,
    /// Pre-authenticated and short-lived; never log it.
    /// OneDrive Personal may spell it `@content.downloadUrl` (the older
    /// OneDrive API name).
    #[serde(default, rename = "@microsoft.graph.downloadUrl", alias = "@content.downloadUrl")]
    pub download_url: Option<String>,
}

/// `createUploadSession`'s answer.
#[derive(Debug, Clone, Deserialize)]
pub struct UploadSession {
    /// Pre-authenticated; never log it.
    #[serde(rename = "uploadUrl")]
    pub upload_url: String,
}

/// An RFC 3339 time as Unix milliseconds.
/// [`DriveItem::content_version`] of a cTag.
pub fn content_version(ctag: &str) -> Option<String> {
    let n = ctag.trim_matches('"').rsplit_once(',')?.1;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| format!("{n}.0"))
}

fn parse_time_ms(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok()
}

/// One version of a file in OneDrive's version history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    pub id: String,
    /// Unix milliseconds.
    pub modified_ms: Option<i64>,
    pub size: Option<i64>,
    /// Who saved it, as OneDrive names them.
    pub by: Option<String>,
}

impl FileVersion {
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            modified_ms: v.get("lastModifiedDateTime").and_then(|t| t.as_str()).and_then(parse_time_ms),
            size: v.get("size").and_then(serde_json::Value::as_i64),
            by: v.pointer("/lastModifiedBy/user/displayName").and_then(|n| n.as_str()).map(str::to_string),
        })
    }
}

/// A folder someone shared with the signed-in account
/// ([`crate::Graph::shared_folders`]): where it really is (another
/// person's drive) and who shared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFolder {
    pub name: String,
    pub drive_id: String,
    pub id: String,
    pub owner: Option<String>,
}

impl SharedFolder {
    /// From one `sharedWithMe` entry; `None` for files and anything
    /// incomplete.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let r = v.get("remoteItem")?;
        r.get("folder")?;
        let s = |p: &str| r.pointer(p).and_then(|x| x.as_str()).map(str::to_string);
        Some(Self {
            name: s("/name").or_else(|| v.get("name").and_then(|n| n.as_str()).map(str::to_string))?,
            drive_id: s("/parentReference/driveId")?,
            id: s("/id")?,
            owner: s("/shared/owner/user/displayName").or_else(|| s("/createdBy/user/displayName")),
        })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default, rename = "eTag")]
    pub e_tag: Option<String>,
    #[serde(default)]
    pub parent_reference: Option<ParentReference>,
    #[serde(default)]
    pub file: Option<FileFacet>,
    #[serde(default)]
    pub folder: Option<FolderFacet>,
    #[serde(default)]
    pub package: Option<Facet>,
    #[serde(default)]
    pub deleted: Option<Facet>,
    #[serde(default)]
    pub root: Option<Facet>,
    #[serde(default)]
    pub file_system_info: Option<FileSystemInfo>,
    #[serde(default)]
    pub shared: Option<SharedFacet>,
    /// Content tag (`c:{…},3`): on OneDrive Personal its count is the
    /// version history's ID for this content ([`DriveItem::content_version`]).
    #[serde(default, rename = "cTag")]
    pub c_tag: Option<String>,
}

/// An item's `shared` facet: Graph's sign that it was shared with others.
/// Who can open it and how is in its permissions
/// ([`crate::Graph::sharing`]); the facet's `scope` is deprecated.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SharedFacet {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub shared_date_time: Option<String>,
    #[serde(default)]
    pub shared_by: Option<Owner>,
}

impl SharedFacet {
    /// When it was shared (Unix milliseconds).
    pub fn shared_ms(&self) -> Option<i64> {
        parse_time_ms(self.shared_date_time.as_deref()?)
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParentReference {
    #[serde(default)]
    pub id: Option<String>,
    /// Only present outside delta responses, e.g. `/drive/root:/a/b`.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    #[serde(default)]
    pub hashes: Option<Hashes>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    #[serde(default)]
    pub quick_xor_hash: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FolderFacet {
    #[serde(default)]
    pub child_count: Option<i64>,
}

/// A facet whose presence matters but whose contents we ignore.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Facet {}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemInfo {
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
}

impl DriveItem {
    pub fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }

    pub fn is_root(&self) -> bool {
        self.root.is_some()
    }

    pub fn is_folder(&self) -> bool {
        self.folder.is_some()
    }

    pub fn is_package(&self) -> bool {
        self.package.is_some()
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.parent_reference.as_ref()?.id.as_deref()
    }

    /// The version its history lists this content as (`3.0` for
    /// `c:{…},3`): OneDrive Personal's cTag counts content changes as the
    /// history numbers them, while the eTag counts renames and moves too
    /// (measured).
    pub fn content_version(&self) -> Option<String> {
        content_version(self.c_tag.as_deref()?)
    }

    /// The decoded 20-byte QuickXorHash, if present and well-formed.
    pub fn quick_xor_hash(&self) -> Option<[u8; 20]> {
        let b64 = self.file.as_ref()?.hashes.as_ref()?.quick_xor_hash.as_ref()?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        bytes.try_into().ok()
    }

    /// Client-side last-modified time as Unix milliseconds.
    pub fn modified_ms(&self) -> Option<i64> {
        parse_time_ms(self.file_system_info.as_ref()?.last_modified_date_time.as_ref()?)
    }

    /// `Some(when)` if the item carries a `shared` facet, with when it was
    /// shared if Graph said. Any facet counts, even one that is empty or
    /// names only the owner: Graph says a non-null facet means shared, and
    /// what the facet holds varies (it's empty for "unique permissions,
    /// not shared" only with `Prefer: hierarchicalsharing`, which isn't
    /// sent). The audit's permissions read drops the false ones.
    pub fn sharing(&self) -> Option<Option<i64>> {
        self.shared.as_ref().map(SharedFacet::shared_ms)
    }
}

/// One way an item is shared: a sharing link, or access granted to people
/// (`permission` resource). Only what the sharing audit shows and checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    pub id: String,
    /// `read`, `write`, `owner`.
    pub roles: Vec<String>,
    /// For a sharing link: `view`, `edit` or `embed`.
    pub link_type: Option<String>,
    /// For a sharing link: `anonymous` (anyone with it), `users` (people
    /// chosen), `organization`, `existingAccess`.
    pub link_scope: Option<String>,
    /// The link itself. Anyone with it can open the item: never log it.
    pub link_url: Option<String>,
    /// Whom it grants access to: names, or e-mail addresses for invitations
    /// not yet accepted. Personal: never log them.
    pub granted_to: Vec<String>,
    /// When it stops working (Unix milliseconds); `None` if never (Graph
    /// says so with the earliest possible date).
    pub expires_ms: Option<i64>,
    /// The folder it comes from, if it's inherited from one.
    pub inherited_from: Option<String>,
    pub has_password: bool,
}

impl Permission {
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).map(str::to_string);
        let roles = v
            .get("roles")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|r| r.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let who = |i: &serde_json::Value| {
            ["/user/displayName", "/user/email", "/siteUser/displayName", "/group/displayName"]
                .iter()
                .find_map(|p| i.pointer(p).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string))
        };
        let mut names: Vec<String> = ["grantedToV2", "grantedTo"].iter().filter_map(|k| who(v.get(*k)?)).collect();
        for key in ["grantedToIdentitiesV2", "grantedToIdentities"] {
            names.extend(v.get(key).and_then(|a| a.as_array()).into_iter().flatten().filter_map(who));
        }
        if names.is_empty() {
            names.extend(s("/invitation/email"));
        }
        let mut granted_to: Vec<String> = Vec::new();
        for n in names {
            if !granted_to.contains(&n) {
                granted_to.push(n);
            }
        }
        Some(Self {
            id: s("/id")?,
            roles,
            link_type: s("/link/type"),
            link_scope: s("/link/scope"),
            link_url: s("/link/webUrl"),
            granted_to,
            expires_ms: s("/expirationDateTime").as_deref().and_then(parse_time_ms).filter(|&ms| ms > 0),
            inherited_from: s("/inheritedFrom/id"),
            has_password: v.get("hasPassword").and_then(serde_json::Value::as_bool).unwrap_or(false),
        })
    }

    /// The owner's own access, which can't be taken away.
    pub fn is_owner(&self) -> bool {
        self.roles.iter().any(|r| r == "owner")
    }

    /// Whether it lets people change the item.
    pub fn can_edit(&self) -> bool {
        self.link_type.as_deref() == Some("edit") || self.roles.iter().any(|r| r == "write")
    }

    /// Whether anyone who has it can open the item, without signing in.
    pub fn is_anyone(&self) -> bool {
        self.link_scope.as_deref() == Some("anonymous")
    }
}

/// An item's sharing as OneDrive has it now ([`crate::Graph::sharing`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemSharing {
    /// Its eTag when read.
    pub etag: String,
    /// The drive's root, whose permissions OneDrive Personal won't change.
    pub root: bool,
    pub permissions: Vec<Permission>,
}

/// An item someone shared with the signed-in account (`sharedWithMe`):
/// where it really is (another person's drive) and who shared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedItem {
    pub name: String,
    pub drive_id: String,
    pub id: String,
    pub folder: bool,
    pub size: Option<i64>,
    pub owner: Option<String>,
    /// When it was shared (Unix milliseconds), if Graph said.
    pub shared_ms: Option<i64>,
}

impl SharedItem {
    /// From one `sharedWithMe` entry; `None` if incomplete.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let r = v.get("remoteItem")?;
        let s = |p: &str| r.pointer(p).and_then(|x| x.as_str()).map(str::to_string);
        Some(Self {
            name: s("/name").or_else(|| v.get("name").and_then(|n| n.as_str()).map(str::to_string))?,
            drive_id: s("/parentReference/driveId")?,
            id: s("/id")?,
            folder: r.get("folder").is_some(),
            size: r.get("size").and_then(serde_json::Value::as_i64),
            owner: s("/shared/owner/user/displayName")
                .or_else(|| s("/shared/sharedBy/user/displayName"))
                .or_else(|| s("/createdBy/user/displayName")),
            shared_ms: s("/shared/sharedDateTime").as_deref().and_then(parse_time_ms),
        })
    }
}

/// One page of a collection response (children or delta).
#[derive(Debug, Clone, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink", default)]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink", default)]
    pub delta_link: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As measured on OneDrive Personal: the cTag's count is the version
    /// history's ID.
    #[test]
    fn a_ctag_gives_its_version_id() {
        assert_eq!(content_version("\"c:{00000000-0000-0000-0000-000000000001},3\"").as_deref(), Some("3.0"));
        assert_eq!(content_version("c:{X},12").as_deref(), Some("12.0"));
        assert_eq!(content_version("c:{X}"), None);
        assert_eq!(content_version("c:{X},a1"), None);
    }

    #[test]
    fn shared_folders_are_read_from_their_remote_item() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"id":"L!1","name":"Trip","remoteItem":{"id":"B!7","name":"Trip","folder":{"childCount":3},
                "parentReference":{"driveId":"b0"},"shared":{"owner":{"user":{"displayName":"Sam"}}}}}"#,
        )
        .unwrap();
        let f = SharedFolder::from_json(&v).unwrap();
        assert_eq!(
            (f.name.as_str(), f.drive_id.as_str(), f.id.as_str(), f.owner.as_deref()),
            ("Trip", "b0", "B!7", Some("Sam"))
        );
        let file: serde_json::Value = serde_json::from_str(
            r#"{"remoteItem":{"id":"B!8","name":"a.txt","file":{},"parentReference":{"driveId":"b0"}}}"#,
        )
        .unwrap();
        assert!(SharedFolder::from_json(&file).is_none(), "files can't be paired");
    }

    #[test]
    fn parses_delta_page() {
        let json = r#"{
          "@odata.nextLink": "https://graph.microsoft.com/v1.0/next",
          "value": [
            {"id":"R!1","name":"root","root":{},"folder":{"childCount":2},"size":10},
            {"id":"R!2","name":"a.txt","size":5,"eTag":"\"{X},1\"",
             "parentReference":{"id":"R!1","driveId":"d"},
             "file":{"mimeType":"text/plain","hashes":{"quickXorHash":"SgAAAAAAAAAAAAAAAQAAAAAAAAA="}},
             "fileSystemInfo":{"lastModifiedDateTime":"2026-10-06T19:40:49Z"}},
            {"id":"R!3","deleted":{"state":"deleted"},"parentReference":{"id":"R!1"}},
            {"id":"R!4","name":"Notebook","package":{"type":"oneNote"},"parentReference":{"id":"R!1"}}
          ]
        }"#;
        let p: Page = serde_json::from_str(json).unwrap();
        assert_eq!(p.next_link.as_deref(), Some("https://graph.microsoft.com/v1.0/next"));
        assert!(p.value[0].is_root() && p.value[0].is_folder());
        let f = &p.value[1];
        assert_eq!(f.parent_id(), Some("R!1"));
        assert_eq!(f.quick_xor_hash().unwrap()[0], 0x4A);
        assert_eq!(f.modified_ms(), Some(1_791_315_649_000));
        assert!(p.value[2].is_deleted());
        assert!(p.value[3].is_package());
    }

    #[test]
    fn every_shared_facet_marks_a_candidate() {
        let item = |json: &str| serde_json::from_str::<DriveItem>(json).unwrap();
        let shared = item(
            r#"{"id":"a","shared":{"owner":{"user":{"displayName":"Me"}},"scope":"users",
                "sharedDateTime":"2026-10-06T19:40:49Z"}}"#,
        );
        assert_eq!(shared.sharing(), Some(Some(1_791_315_649_000)));
        assert_eq!(item(r#"{"id":"b","shared":{"scope":"anonymous"}}"#).sharing(), Some(None));
        assert_eq!(item(r#"{"id":"c"}"#).sharing(), None);
        // Any facet is a candidate, even an empty one or one naming only
        // the owner: the permissions read tells.
        assert_eq!(item(r#"{"id":"d","shared":{}}"#).sharing(), Some(None));
        assert_eq!(item(r#"{"id":"e","shared":{"owner":{"user":{"displayName":"Me"}}}}"#).sharing(), Some(None));
    }

    #[test]
    fn permissions_are_read_as_links_and_people() {
        let p = |json: &str| Permission::from_json(&serde_json::from_str(json).unwrap()).unwrap();
        let link = p(r#"{"id":"1","roles":["write"],"link":{"type":"edit","scope":"anonymous",
            "webUrl":"https://1drv.ms/x"},"expirationDateTime":"0001-01-01T00:00:00Z","hasPassword":true}"#);
        assert!(link.can_edit() && link.is_anyone() && link.has_password && !link.is_owner());
        assert_eq!((link.expires_ms, link.inherited_from.as_deref()), (None, None), "the earliest date is never");
        let people = p(r#"{"id":"2","roles":["read"],"link":{"type":"view","scope":"users"},
            "grantedToIdentitiesV2":[{"user":{"displayName":"Sam"}},{"user":{"displayName":"Ana"}}],
            "grantedToIdentities":[{"user":{"displayName":"Sam"}}],
            "expirationDateTime":"2026-11-01T00:00:00Z","inheritedFrom":{"id":"F!1"}}"#);
        assert_eq!(people.granted_to, ["Sam", "Ana"]);
        assert!(!people.can_edit() && !people.is_anyone());
        assert_eq!(people.expires_ms, Some(1_793_491_200_000));
        assert_eq!(people.inherited_from.as_deref(), Some("F!1"));
        let invited = p(r#"{"id":"3","roles":["write"],"invitation":{"email":"x@example.com"}}"#);
        assert_eq!(invited.granted_to, ["x@example.com"]);
        assert!(p(r#"{"id":"4","roles":["owner"]}"#).is_owner());
        assert!(Permission::from_json(&serde_json::json!({"roles":["read"]})).is_none(), "no id");
    }

    #[test]
    fn items_shared_with_me_are_files_and_folders() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"id":"L!1","remoteItem":{"id":"B!8","name":"a.txt","file":{},"size":5,
                "parentReference":{"driveId":"b0"},
                "shared":{"owner":{"user":{"displayName":"Sam"}},"sharedDateTime":"2026-10-06T19:40:49Z"}}}"#,
        )
        .unwrap();
        let it = SharedItem::from_json(&v).unwrap();
        assert_eq!((it.name.as_str(), it.folder, it.owner.as_deref()), ("a.txt", false, Some("Sam")));
        assert_eq!((it.size, it.shared_ms), (Some(5), Some(1_791_315_649_000)));
        assert!(SharedItem::from_json(&serde_json::json!({"remoteItem":{"id":"x"}})).is_none());
    }

    #[test]
    fn malformed_hash_is_none() {
        let it: DriveItem = serde_json::from_str(r#"{"id":"x","file":{"hashes":{"quickXorHash":"bad!"}}}"#).unwrap();
        assert_eq!(it.quick_xor_hash(), None);
    }
}
