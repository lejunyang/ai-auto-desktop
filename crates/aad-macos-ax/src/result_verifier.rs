//! Safe verification of returned macOS AX fixture archives.
//!
//! The verifier never extracts the archive. It bounds the compressed input,
//! decompressed tar, individual members and aggregate member bytes, then checks
//! normalized archive metadata before trusting any report content.

use flate2::{Decompress, FlushDecompress, Status};
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{Metadata, OpenOptions};
use std::io::Read;
use std::path::Path;

const VERIFIER_SCHEMA: &str = "ai-auto-desktop.macos-result-verifier/v1";
const EXPECTED_MEMBERS: [&str; 4] = ["report.json", "README.txt", "identity.txt", "SHA256SUMS"];
const HASHED_MEMBERS: [&str; 3] = ["report.json", "README.txt", "identity.txt"];
const MAX_COMPRESSED_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TAR_BYTES: usize = 4 * 1024 * 1024;
const NORMALIZED_MTIME: u64 = 946_684_800;
const REQUIRED_CHECKS: [&str; 10] = [
    "screen_capture_preflight",
    "accessibility_trust",
    "bounded_discovery",
    "roles_and_ambiguity",
    "type_text_secure_rejected",
    "focus_and_reread",
    "set_value_and_reread",
    "type_text_unicode_and_reread",
    "press_and_reread",
    "pointer_click_and_reread",
];

#[derive(Default)]
pub struct VerifyOptions {
    pub expected_archive_sha256: Option<String>,
    pub expected_source_revision: Option<String>,
    pub expected_source_package_digest: Option<String>,
}

#[derive(Debug)]
pub struct VerifyError {
    code: String,
    message: String,
    details: Map<String, Value>,
}

impl VerifyError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            details: Map::new(),
        }
    }

    fn detail(mut self, key: &str, value: Value) -> Self {
        self.details.insert(key.to_string(), value);
        self
    }

    pub fn document(&self) -> Value {
        let mut error = Map::from_iter([
            ("code".into(), json!(self.code)),
            ("message".into(), json!(self.message)),
        ]);
        if !self.details.is_empty() {
            error.insert("details".into(), Value::Object(self.details.clone()));
        }
        failure(Value::Object(error))
    }
}

pub fn failure_document(code: &str, message: &str) -> Value {
    failure(json!({"code": code, "message": message}))
}

fn failure(error: Value) -> Value {
    json!({
        "schema_version": VERIFIER_SCHEMA,
        "status": "failed",
        "archive_valid": false,
        "verified_archive": false,
        "report_passed": false,
        "trusted_archive": false,
        "source_trusted": false,
        "qualified": false,
        "error": error,
    })
}

pub fn verify(path: &Path, options: &VerifyOptions) -> Result<Value, VerifyError> {
    let (compressed, metadata) = read_archive(path)?;
    let archive_hash = hex_sha256(&compressed);
    let tar = decompress(&compressed)?;
    let members = parse_ustar(&tar)?;
    verify_manifest(&members)?;
    decode_text(require_member(&members, "README.txt")?, "README.txt")?;
    let report = validate_report(require_member(&members, "report.json")?)?;
    let report_passed = report["status"] == "passed";
    let identity = if report_passed {
        parse_identity(require_member(&members, "identity.txt")?)?
    } else {
        inspect_nonpassed_identity(require_member(&members, "identity.txt")?)?
    };

    let source = report.get("source").cloned().unwrap_or(Value::Null);
    let identity_source = identity.get("source").cloned().unwrap_or(Value::Null);
    if source.is_null() != identity_source.is_null()
        || (!source.is_null() && source != identity_source)
    {
        return Err(VerifyError::new(
            "source_provenance_mismatch",
            "report 与 identity 的 source provenance 不一致。",
        ));
    }

    if report_passed {
        let stability = report["identity"]["launcher_declared_identity_stability"]
            .as_str()
            .ok_or_else(|| invalid_report("identity stability 缺失。"))?;
        if identity["stability"] != stability {
            return Err(VerifyError::new(
                "identity_stability_mismatch",
                "report 与 identity stability 不一致。",
            ));
        }
        let architecture = report["platform"]["architecture"]
            .as_str()
            .ok_or_else(|| invalid_report("platform architecture 缺失。"))?;
        for section in ["runner", "fixture"] {
            let has_architecture = identity[section]["architectures"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value == architecture));
            if !has_architecture {
                return Err(VerifyError::new(
                    "identity_architecture_mismatch",
                    "report architecture 不在 identity 架构中。",
                )
                .detail("section", json!(section)));
            }
        }
    }

    let trusted_archive = match options.expected_archive_sha256.as_deref() {
        None => false,
        Some(expected) if !is_hex(expected, 64, false) => false,
        Some(expected) => expected.eq_ignore_ascii_case(&archive_hash),
    };
    let mut trust_error = match options.expected_archive_sha256.as_deref() {
        Some(expected) if !is_hex(expected, 64, false) => Some(json!({
            "code": "invalid_expected_archive_sha256",
            "message": "受信任的预期归档 SHA-256 必须是 64 位十六进制。"
        })),
        Some(_) if !trusted_archive => Some(json!({
            "code": "archive_sha256_mismatch",
            "message": "归档 SHA-256 与受信任预期值不一致。"
        })),
        _ => None,
    };

    let source_requested = options.expected_source_revision.is_some()
        || options.expected_source_package_digest.is_some();
    let mut source_matches = false;
    if trust_error.is_none() && source_requested {
        match (
            options.expected_source_revision.as_deref(),
            options.expected_source_package_digest.as_deref(),
        ) {
            (None, _) | (_, None) => {
                trust_error = Some(trust_error_value(
                    "incomplete_expected_source",
                    "可信源码预期值必须同时提供 revision 和 package digest。",
                ))
            }
            (Some(revision), _) if !is_revision(revision) => {
                trust_error = Some(trust_error_value(
                    "invalid_expected_source_revision",
                    "受信任的预期源码 revision 必须是小写 Git commit SHA。",
                ))
            }
            (_, Some(digest)) if !is_hex(digest, 64, true) => {
                trust_error = Some(trust_error_value(
                    "invalid_expected_source_package_digest",
                    "受信任的预期源码 package digest 必须是 64 位小写十六进制。",
                ))
            }
            (Some(revision), Some(digest)) if trusted_archive => {
                if source.is_null() {
                    trust_error = Some(trust_error_value(
                        "source_provenance_missing",
                        "旧报告未携带可校验的源码 provenance；拒绝源码信任。",
                    ));
                } else if source["revision"] != revision {
                    trust_error = Some(trust_error_value(
                        "source_revision_mismatch",
                        "报告源码 revision 与受信任预期值不一致。",
                    ));
                } else if source["package_digest"] != digest {
                    trust_error = Some(trust_error_value(
                        "source_package_digest_mismatch",
                        "报告源码 package digest 与受信任预期值不一致。",
                    ));
                } else if source["worktree"] != "clean" {
                    trust_error = Some(trust_error_value(
                        "source_worktree_dirty",
                        "dirty 开发源码包不能用于资格认定。",
                    ));
                } else {
                    source_matches = true;
                }
            }
            _ => {}
        }
    }
    let source_trusted = trusted_archive && source_matches;
    let qualified = report_passed && trusted_archive && source_trusted;
    let mut result = json!({
        "schema_version": VERIFIER_SCHEMA,
        "status": if qualified { "passed" } else { "failed" },
        "archive_valid": true,
        "verified_archive": true,
        "report_passed": report_passed,
        "trusted_archive": trusted_archive,
        "source_trusted": source_trusted,
        "qualified": qualified,
        "archive": {
            "sha256": archive_hash,
            "size_bytes": metadata.len(),
            "members": EXPECTED_MEMBERS,
        },
        "report": {
            "status": report["status"],
            "architecture": report["platform"]["architecture"],
            "checks_passed": report["summary"]["passed"],
        },
        "identity": identity,
        "source": source,
    });
    let error = trust_error.or_else(|| {
        if !report_passed {
            Some(json!({"code": "report_not_passed", "message": "macOS 真机报告未通过。", "details": {"report_status": report["status"]}}))
        } else if !trusted_archive {
            Some(trust_error_value("untrusted_archive", "报告内容通过，但未提供独立可信的归档 SHA-256。"))
        } else if !source_trusted {
            Some(trust_error_value("untrusted_source", "未同时提供独立可信的源码 revision 与 package digest。"))
        } else {
            None
        }
    });
    if let Some(error) = error {
        result["error"] = error;
    }
    Ok(result)
}

fn read_archive(path: &Path) -> Result<(Vec<u8>, Metadata), VerifyError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| VerifyError::new("input_unreadable", "无法读取结果归档元数据。"))?;
    if metadata.file_type().is_symlink() {
        return Err(VerifyError::new(
            "input_symlink",
            "结果归档路径不能是符号链接。",
        ));
    }
    if !metadata.is_file() {
        return Err(VerifyError::new(
            "input_not_regular",
            "结果归档必须是普通文件。",
        ));
    }
    if metadata.len() == 0 {
        return Err(VerifyError::new("input_empty", "结果归档为空。"));
    }
    if metadata.len() > MAX_COMPRESSED_BYTES {
        return Err(VerifyError::new(
            "archive_too_large",
            "压缩归档超过硬上限。",
        ));
    }
    let mut open = OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = open
        .open(path)
        .map_err(|_| VerifyError::new("input_unreadable", "无法安全打开结果归档。"))?;
    let _opened_metadata = file
        .metadata()
        .map_err(|_| VerifyError::new("input_unreadable", "无法读取打开后的归档元数据。"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (metadata.dev(), metadata.ino()) != (_opened_metadata.dev(), _opened_metadata.ino()) {
            return Err(VerifyError::new(
                "input_changed",
                "结果归档在打开期间发生替换。",
            ));
        }
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_COMPRESSED_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| VerifyError::new("input_unreadable", "无法读取结果归档。"))?;
    if bytes.len() as u64 > MAX_COMPRESSED_BYTES {
        return Err(VerifyError::new(
            "archive_too_large",
            "压缩归档超过硬上限。",
        ));
    }
    let final_metadata = file
        .metadata()
        .map_err(|_| VerifyError::new("input_changed", "无法复核结果归档。"))?;
    if final_metadata.len() != bytes.len() as u64
        || final_metadata.modified().ok() != _opened_metadata.modified().ok()
    {
        return Err(VerifyError::new(
            "input_changed",
            "结果归档在读取期间发生变化。",
        ));
    }
    Ok((bytes, final_metadata))
}

fn decompress(compressed: &[u8]) -> Result<Vec<u8>, VerifyError> {
    if compressed.len() < 18 || &compressed[..3] != b"\x1f\x8b\x08" {
        return Err(VerifyError::new(
            "invalid_gzip",
            "归档不是 gzip deflate 数据。",
        ));
    }
    if compressed[3] != 0
        || compressed[4..8] != [0, 0, 0, 0]
        || compressed[8] != 0
        || compressed[9] != 3
    {
        return Err(VerifyError::new(
            "non_normalized_gzip",
            "gzip header 未规范化。",
        ));
    }
    let trailer = &compressed[compressed.len() - 8..];
    let deflate = &compressed[10..compressed.len() - 8];
    let mut decoder = Decompress::new(false);
    let mut tar = Vec::new();
    let mut input = deflate;
    loop {
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let mut chunk = [0u8; 64 * 1024];
        let status = match decoder.decompress(input, &mut chunk, FlushDecompress::None) {
            Ok(status) => status,
            Err(error) if decoder.total_out() as usize > MAX_TAR_BYTES => {
                let _ = error;
                return Err(VerifyError::new(
                    "tar_payload_too_large",
                    "gzip 解压后的 tar 数据超过硬上限。",
                ));
            }
            Err(_) => {
                return Err(VerifyError::new(
                    "invalid_gzip",
                    "归档不是完整有效的 gzip 数据。",
                ))
            }
        };
        let consumed = (decoder.total_in() - before_in) as usize;
        let produced = (decoder.total_out() - before_out) as usize;
        tar.extend_from_slice(&chunk[..produced]);
        if tar.len() > MAX_TAR_BYTES {
            return Err(VerifyError::new(
                "tar_payload_too_large",
                "gzip 解压后的 tar 数据超过硬上限。",
            ));
        }
        input = &input[consumed..];
        if status == Status::StreamEnd {
            if !input.is_empty() {
                return Err(VerifyError::new(
                    "invalid_gzip",
                    "归档必须恰好包含一个完整 gzip member。",
                ));
            }
            break;
        }
        if consumed == 0 && produced == 0 {
            return Err(VerifyError::new(
                "invalid_gzip",
                "归档不是完整有效的 gzip 数据。",
            ));
        }
    }
    let expected_crc = u32::from_le_bytes(trailer[..4].try_into().unwrap());
    let expected_size = u32::from_le_bytes(trailer[4..].try_into().unwrap());
    if crc32fast::hash(&tar) != expected_crc || tar.len() as u32 != expected_size {
        return Err(VerifyError::new(
            "invalid_gzip",
            "gzip checksum 或 size 校验失败。",
        ));
    }
    Ok(tar)
}

fn parse_ustar(tar: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, VerifyError> {
    if tar.len() < 1536 || tar.len() % 512 != 0 {
        return Err(VerifyError::new("invalid_tar", "tar 长度或结束块无效。"));
    }
    let mut members = BTreeMap::new();
    let mut order = Vec::new();
    let mut total = 0usize;
    let mut offset = 0usize;
    let mut ended = false;
    while offset + 512 <= tar.len() {
        let header = &tar[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            if offset + 1024 > tar.len()
                || tar[offset + 512..offset + 1024]
                    .iter()
                    .any(|byte| *byte != 0)
                || tar[offset + 1024..].iter().any(|byte| *byte != 0)
            {
                return Err(VerifyError::new("invalid_tar", "tar 结束块无效。"));
            }
            ended = true;
            break;
        }
        let stored = tar_octal(&header[148..156], "checksum")?;
        let calculated = header[..148]
            .iter()
            .chain(header[156..].iter())
            .map(|value| u64::from(*value))
            .sum::<u64>()
            + 8 * u64::from(b' ');
        if stored != calculated || &header[257..263] != b"ustar\0" || &header[263..265] != b"00" {
            return Err(VerifyError::new(
                "invalid_tar_header",
                "tar header 或 ustar 格式无效。",
            ));
        }
        let name = tar_name(header)?;
        if !safe_member(&name) {
            return Err(
                VerifyError::new("unsafe_member_path", "归档成员路径不安全。")
                    .detail("member", json!(name)),
            );
        }
        if members.contains_key(&name) {
            return Err(VerifyError::new("duplicate_member", "归档包含重复成员。")
                .detail("member", json!(name)));
        }
        if !EXPECTED_MEMBERS.contains(&name.as_str()) {
            return Err(
                VerifyError::new("extra_member", "归档包含白名单以外的成员。")
                    .detail("member", json!(name)),
            );
        }
        if header[156] != 0 && header[156] != b'0' {
            let kind = match header[156] {
                b'1' => "hardlink",
                b'2' => "symlink",
                b'3' => "character_device",
                b'4' => "block_device",
                b'5' => "directory",
                b'6' => "fifo",
                _ => "special",
            };
            return Err(
                VerifyError::new("unsafe_member_type", "归档成员必须是普通文件。")
                    .detail("member", json!(name))
                    .detail("type", json!(kind)),
            );
        }
        let mode = tar_octal(&header[100..108], "mode")?;
        let uid = tar_octal(&header[108..116], "uid")?;
        let gid = tar_octal(&header[116..124], "gid")?;
        let mtime = tar_octal(&header[136..148], "mtime")?;
        let uname = tar_text(&header[265..297], "uname")?;
        let gname = tar_text(&header[297..329], "gname")?;
        if mode != 0o644
            || uid != 0
            || gid != 0
            || mtime != NORMALIZED_MTIME
            || !matches!(
                (uname.as_str(), gname.as_str()),
                ("", "") | ("root", "root")
            )
        {
            return Err(VerifyError::new(
                "non_normalized_tar_metadata",
                "归档成员 metadata 未规范化。",
            )
            .detail("member", json!(name)));
        }
        let size = tar_octal(&header[124..136], "size")? as usize;
        let limit = member_limit(&name);
        if size > limit {
            return Err(VerifyError::new("member_too_large", "归档成员超过硬上限。")
                .detail("member", json!(name)));
        }
        total += size;
        if total > 836 * 1024 {
            return Err(VerifyError::new(
                "members_too_large",
                "归档成员总大小超过硬上限。",
            ));
        }
        let start = offset + 512;
        let end = start
            .checked_add(size)
            .ok_or_else(|| VerifyError::new("truncated_member", "归档成员数据被截断。"))?;
        let next = start + size.div_ceil(512) * 512;
        if end > tar.len() || next > tar.len() {
            return Err(VerifyError::new("truncated_member", "归档成员数据被截断。"));
        }
        if tar[end..next].iter().any(|byte| *byte != 0) {
            return Err(VerifyError::new(
                "invalid_tar_padding",
                "归档成员 padding 包含非零数据。",
            ));
        }
        members.insert(name.clone(), tar[start..end].to_vec());
        order.push(name);
        offset = next;
    }
    if !ended {
        return Err(VerifyError::new("invalid_tar", "tar 缺少结束块。"));
    }
    let missing: Vec<_> = EXPECTED_MEMBERS
        .iter()
        .filter(|name| !members.contains_key(**name))
        .collect();
    if !missing.is_empty() {
        return Err(VerifyError::new("missing_member", "归档缺少必需成员。")
            .detail("members", json!(missing)));
    }
    if order != EXPECTED_MEMBERS {
        return Err(VerifyError::new(
            "non_normalized_tar_order",
            "归档成员顺序未按生成器规范固定。",
        )
        .detail("members", json!(order)));
    }
    Ok(members)
}

fn verify_manifest(members: &BTreeMap<String, Vec<u8>>) -> Result<(), VerifyError> {
    let text = decode_text(require_member(members, "SHA256SUMS")?, "SHA256SUMS")?;
    if !text.ends_with('\n') || text.lines().count() != HASHED_MEMBERS.len() {
        return Err(VerifyError::new(
            "invalid_manifest",
            "SHA256SUMS 必须恰好包含三行。",
        ));
    }
    let mut seen = BTreeSet::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let hash = fields.next().unwrap_or("");
        let name = fields.next().unwrap_or("");
        if fields.next().is_some()
            || !is_hex(hash, 64, false)
            || !HASHED_MEMBERS.contains(&name)
            || !seen.insert(name)
        {
            return Err(VerifyError::new(
                "invalid_manifest",
                "SHA256SUMS 行格式、文件名或唯一性无效。",
            ));
        }
        if !hash.eq_ignore_ascii_case(&hex_sha256(require_member(members, name)?)) {
            return Err(
                VerifyError::new("hash_mismatch", "归档成员 SHA-256 不匹配。")
                    .detail("member", json!(name)),
            );
        }
    }
    Ok(())
}

fn validate_report(data: &[u8]) -> Result<Value, VerifyError> {
    let report = parse_json_no_duplicates(decode_text(data, "report.json")?)?;
    let object = report
        .as_object()
        .ok_or_else(|| invalid_report("report 顶层必须是对象。"))?;
    if object.get("schema_version") != Some(&json!("1.0"))
        || object.get("kind") != Some(&json!("macos_ax_fixture_test"))
    {
        return Err(invalid_report("report schema_version 或 kind 不受支持。"));
    }
    let status = required_string(object, "status")?;
    if !["passed", "failed", "unsupported"].contains(&status) {
        return Err(VerifyError::new(
            "invalid_report_status",
            "report status 无效。",
        ));
    }
    required_string(object, "message")?;
    let checks = object
        .get("checks")
        .and_then(Value::as_array)
        .ok_or_else(|| VerifyError::new("invalid_report_checks", "report checks 必须是数组。"))?;
    let mut ids = BTreeSet::new();
    let mut passed = 0u64;
    let mut failed = 0u64;
    for check in checks {
        let check = check
            .as_object()
            .ok_or_else(|| VerifyError::new("invalid_report_checks", "每个 check 必须是对象。"))?;
        let id = required_string(check, "id")?;
        if !ids.insert(id.to_string()) {
            return Err(VerifyError::new(
                "invalid_report_checks",
                "report check id 重复。",
            ));
        }
        match required_string(check, "status")? {
            "pass" => passed += 1,
            "fail" => failed += 1,
            "unsupported" => {}
            _ => {
                return Err(VerifyError::new(
                    "invalid_report_checks",
                    "report check status 无效。",
                ))
            }
        }
        required_string(check, "message")?;
    }
    let summary = object
        .get("summary")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_report("summary 必须是对象。"))?;
    if nonnegative(summary, "passed")? != passed
        || nonnegative(summary, "failed")? != failed
        || nonnegative(summary, "total")? != checks.len() as u64
    {
        return Err(VerifyError::new(
            "invalid_report_summary",
            "report summary 与 checks 不一致。",
        ));
    }
    let platform = object
        .get("platform")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_report("platform 必须是对象。"))?;
    let os = required_string(platform, "os")?;
    let architecture = required_string(platform, "architecture")?;
    if !["arm64", "x86_64", "unknown"].contains(&architecture)
        || (status == "passed" && architecture == "unknown")
    {
        return Err(VerifyError::new(
            "invalid_report_architecture",
            "report architecture 不受支持。",
        ));
    }
    if status == "passed" && os != "macos" {
        return Err(VerifyError::new(
            "invalid_report_platform",
            "passed report 必须来自 macOS。",
        ));
    }
    if status == "passed" {
        required_string(platform, "version")?;
        boolean(platform, "rosetta_translated")?;
    }
    validate_launcher(object)?;
    validate_permissions(object, status)?;
    if let Some(source) = object.get("source") {
        validate_source(source)?;
    }
    if status == "passed" {
        validate_passed_report(object, checks, &ids)?;
    } else if let Some(limits) = object.get("limits") {
        validate_limits(limits)?;
    }
    if status == "passed" || object.contains_key("timestamp_utc") {
        let timestamp = required_string(object, "timestamp_utc")?;
        if !timestamp_shape(timestamp) {
            return Err(invalid_report("timestamp_utc 格式无效。"));
        }
    }
    Ok(report)
}

fn validate_launcher(report: &Map<String, Value>) -> Result<(), VerifyError> {
    match (report.get("execution"), report.get("error")) {
        (None, None) => return Ok(()),
        (Some(_), Some(_)) => {}
        _ => return Err(invalid_report("execution 与 error 必须同时出现。")),
    }
    let execution = report["execution"]
        .as_object()
        .ok_or_else(|| invalid_report("execution 必须是对象。"))?;
    let phase = required_string(execution, "phase")?;
    let status = nonnegative(execution, "command_status")?;
    let timed_out = boolean(execution, "timed_out")?;
    let timeout = nonnegative(execution, "timeout_seconds")?;
    boolean(execution, "runner_pid_observed")?;
    if !["build", "runner", "archive"].contains(&phase)
        || status > 255
        || !(1..=600).contains(&timeout)
    {
        return Err(invalid_report("launcher diagnostics 超出范围。"));
    }
    let error = report["error"]
        .as_object()
        .ok_or_else(|| invalid_report("error 必须是对象。"))?;
    let code = required_string(error, "code")?;
    required_string(error, "message")?;
    if report["status"] == "passed"
        || (timed_out && (phase != "runner" || status != 124))
        || (code == "runner_timeout" && !timed_out)
    {
        return Err(invalid_report("launcher diagnostics 与报告状态不一致。"));
    }
    Ok(())
}

fn validate_permissions(report: &Map<String, Value>, status: &str) -> Result<(), VerifyError> {
    let permissions = report
        .get("permissions")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_report("permissions 必须是对象。"))?;
    let accessibility = permissions
        .get("accessibility")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_report("accessibility 必须是对象。"))?;
    if status == "passed" && accessibility.get("trusted") != Some(&json!(true)) {
        return Err(VerifyError::new(
            "invalid_report_permissions",
            "passed 报告必须已获得 Accessibility trust。",
        ));
    }
    if status != "passed" {
        if accessibility.contains_key("trusted") {
            boolean(accessibility, "trusted")?;
        } else {
            boolean(accessibility, "checked")?;
        }
    }
    if accessibility.contains_key("prompt_requested") {
        boolean(accessibility, "prompt_requested")?;
    }
    let capture = permissions
        .get("screen_capture")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_report("screen_capture 必须是对象。"))?;
    if status == "passed" || capture.contains_key("preflight_granted") {
        boolean(capture, "preflight_granted")?;
    } else {
        boolean(capture, "checked")?;
    }
    if capture.get("request_attempted") != Some(&json!(false))
        || capture.get("capture_attempted") != Some(&json!(false))
    {
        return Err(VerifyError::new(
            "unsafe_report_claim",
            "报告显示曾请求授权或采集屏幕内容。",
        ));
    }
    Ok(())
}

fn validate_passed_report(
    report: &Map<String, Value>,
    checks: &[Value],
    ids: &BTreeSet<String>,
) -> Result<(), VerifyError> {
    if checks.is_empty() || checks.iter().any(|check| check["status"] != "pass") {
        return Err(VerifyError::new(
            "invalid_report_checks",
            "passed 报告的全部 checks 必须通过。",
        ));
    }
    let missing: Vec<_> = REQUIRED_CHECKS
        .iter()
        .filter(|id| !ids.contains(**id))
        .collect();
    if !missing.is_empty() {
        return Err(
            VerifyError::new("missing_required_checks", "passed 报告缺少必需 check。")
                .detail("checks", json!(missing)),
        );
    }
    let pointer = checks
        .iter()
        .find(|check| check["id"] == "pointer_click_and_reread")
        .and_then(|check| check["evidence"].as_object())
        .ok_or_else(|| VerifyError::new("invalid_pointer_evidence", "pointer click 证据缺失。"))?;
    for field in [
        "fresh_target",
        "positive_area_bounds",
        "target_pid_matches_fixture",
        "frontmost_before_dispatch",
        "frontmost_at_dispatch",
        "status_idle_before_dispatch",
        "center_derived_from_ax_bounds",
        "center_finite",
        "hit_test_matches_target",
        "event_submitted",
        "postcondition_reread",
        "status_matches_from_fresh_snapshot",
    ] {
        if pointer.get(field) != Some(&json!(true)) {
            return Err(
                VerifyError::new("invalid_pointer_evidence", "pointer click 证据不完整。")
                    .detail("field", json!(field)),
            );
        }
    }
    if pointer.get("button") != Some(&json!("left"))
        || pointer.get("position") != Some(&json!("center"))
        || pointer.get("pid_ax_error") != Some(&json!(0))
    {
        return Err(VerifyError::new(
            "invalid_pointer_evidence",
            "pointer click 参数或 PID 证据无效。",
        ));
    }
    for field in ["bounds_ax_errors", "postcondition_ax_errors"] {
        if !pointer
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            return Err(VerifyError::new(
                "invalid_pointer_evidence",
                "pointer click AX 错误必须为空。",
            )
            .detail("field", json!(field)));
        }
    }
    let identity = report
        .get("identity")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_report("identity 必须是对象。"))?;
    if identity.get("runner_bundle_id") != Some(&json!("dev.ai-auto-desktop.testkit.ax-runner"))
        || identity.get("fixture_bundle_id") != Some(&json!("dev.ai-auto-desktop.testkit.fixture"))
    {
        return Err(VerifyError::new(
            "invalid_bundle_id",
            "report bundle ID 不匹配。",
        ));
    }
    let stability = required_string(identity, "launcher_declared_identity_stability")?;
    if !["ephemeral", "stable_identity_requested"].contains(&stability) {
        return Err(VerifyError::new(
            "invalid_identity_stability",
            "report identity stability 无效。",
        ));
    }
    validate_limits(
        report
            .get("limits")
            .ok_or_else(|| invalid_report("limits 必须是对象。"))?,
    )?;
    Ok(())
}

fn validate_limits(limits: &Value) -> Result<(), VerifyError> {
    let limits = limits
        .as_object()
        .ok_or_else(|| invalid_report("limits 必须是对象。"))?;
    if limits.get("target_scope") != Some(&json!("fixture_process_only"))
        || limits.get("screen_content_collected") != Some(&json!(false))
    {
        return Err(VerifyError::new(
            "unsafe_report_claim",
            "report target 或 screen content 范围无效。",
        ));
    }
    Ok(())
}

fn parse_identity(data: &[u8]) -> Result<Value, VerifyError> {
    let text = decode_text(data, "identity.txt")?;
    if !text.ends_with('\n') || text.lines().count() > 64 {
        return Err(VerifyError::new(
            "invalid_identity",
            "identity.txt 格式或行数无效。",
        ));
    }
    let mut globals = BTreeMap::new();
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.is_empty() || line.trim() != line {
            return Err(VerifyError::new(
                "invalid_identity",
                "identity.txt 包含空行或行首尾空白。",
            ));
        }
        if line == "[runner]" || line == "[fixture]" {
            let section = line.trim_matches(&['[', ']'][..]).to_string();
            if sections.insert(section.clone(), BTreeMap::new()).is_some() {
                return Err(VerifyError::new(
                    "invalid_identity",
                    "identity section 重复。",
                ));
            }
            current = Some(section);
            continue;
        }
        let (key, value) = if let Some(value) = line.strip_prefix("designated => ") {
            ("designated", value)
        } else {
            line.split_once('=')
                .ok_or_else(|| VerifyError::new("invalid_identity", "identity.txt 行格式无效。"))?
        };
        if key.is_empty() || value.is_empty() {
            return Err(VerifyError::new("invalid_identity", "identity 字段为空。"));
        }
        let destination = match current.as_ref() {
            Some(section) => sections.get_mut(section).unwrap(),
            None => &mut globals,
        };
        if destination
            .insert(key.to_string(), value.to_string())
            .is_some()
        {
            return Err(VerifyError::new("invalid_identity", "identity 字段重复。"));
        }
    }
    let required_globals = BTreeSet::from(["swift", "identity_stability"]);
    let provenance_globals = BTreeSet::from([
        "source_revision",
        "source_worktree",
        "source_package_digest",
    ]);
    let keys: BTreeSet<_> = globals.keys().map(String::as_str).collect();
    let with_provenance = required_globals
        .union(&provenance_globals)
        .copied()
        .collect::<BTreeSet<_>>();
    if keys != required_globals && keys != with_provenance {
        return Err(VerifyError::new(
            "invalid_identity",
            "identity 顶层必填字段不完整。",
        ));
    }
    if sections.len() != 2 || !sections.contains_key("runner") || !sections.contains_key("fixture")
    {
        return Err(VerifyError::new(
            "invalid_identity",
            "identity runner/fixture section 不完整。",
        ));
    }
    let stability = globals["identity_stability"].as_str();
    if !["ephemeral", "stable_identity_requested"].contains(&stability) {
        return Err(VerifyError::new(
            "invalid_identity_stability",
            "identity stability 无效。",
        ));
    }
    let mut parsed = Map::from_iter([
        ("available".into(), json!(true)),
        ("stability".into(), json!(stability)),
    ]);
    for (section_name, expected_id) in [
        ("runner", "dev.ai-auto-desktop.testkit.ax-runner"),
        ("fixture", "dev.ai-auto-desktop.testkit.fixture"),
    ] {
        let fields = &sections[section_name];
        let required = [
            "designated",
            "designated_requirement_origin",
            "Identifier",
            "CDHash",
            "architectures",
            "sha256",
        ];
        let allowed = [
            "designated",
            "designated_requirement_origin",
            "Identifier",
            "TeamIdentifier",
            "CDHash",
            "architectures",
            "sha256",
        ];
        if required.iter().any(|key| !fields.contains_key(*key))
            || fields.keys().any(|key| !allowed.contains(&key.as_str()))
        {
            return Err(VerifyError::new(
                "invalid_identity",
                "identity section 字段不完整或未知。",
            )
            .detail("section", json!(section_name)));
        }
        let origin = fields["designated_requirement_origin"].as_str();
        if !["implicit", "explicit"].contains(&origin) {
            return Err(VerifyError::new(
                "invalid_designated_requirement_origin",
                "designated requirement origin 无效。",
            ));
        }
        if stability == "ephemeral" && origin != "implicit" {
            return Err(VerifyError::new(
                "identity_requirement_origin_mismatch",
                "ephemeral identity 必须使用 implicit designated requirement。",
            ));
        }
        if fields["Identifier"] != expected_id {
            return Err(
                VerifyError::new("invalid_bundle_id", "identity bundle ID 不匹配。")
                    .detail("section", json!(section_name)),
            );
        }
        if !is_hex(&fields["CDHash"], fields["CDHash"].len(), false) || fields["CDHash"].is_empty()
        {
            return Err(VerifyError::new(
                "invalid_cdhash",
                "identity CDHash 格式无效。",
            ));
        }
        if !is_hex(&fields["sha256"], 64, false) {
            return Err(VerifyError::new(
                "invalid_executable_hash",
                "identity executable SHA-256 格式无效。",
            ));
        }
        let architectures: Vec<_> = fields["architectures"].split(' ').collect();
        let unique: BTreeSet<_> = architectures.iter().copied().collect();
        if architectures.is_empty()
            || unique.len() != architectures.len()
            || architectures
                .iter()
                .any(|arch| !["arm64", "x86_64"].contains(arch))
        {
            return Err(VerifyError::new(
                "invalid_identity_architecture",
                "identity architectures 无效。",
            ));
        }
        parsed.insert(
            section_name.to_string(),
            json!({
                "bundle_id": fields["Identifier"],
                "designated_requirement_origin": origin,
                "architectures": architectures,
                "sha256": fields["sha256"].to_ascii_lowercase(),
            }),
        );
    }
    let runner_architectures: BTreeSet<_> = parsed["runner"]["architectures"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let fixture_architectures: BTreeSet<_> = parsed["fixture"]["architectures"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if runner_architectures != fixture_architectures {
        return Err(VerifyError::new(
            "identity_architecture_mismatch",
            "runner 与 fixture 架构集合不一致。",
        ));
    }
    if keys == with_provenance {
        let source = json!({
            "revision": globals["source_revision"],
            "worktree": globals["source_worktree"],
            "package_digest": globals["source_package_digest"],
        });
        validate_source(&source)?;
        parsed.insert("source".into(), source);
    }
    Ok(Value::Object(parsed))
}

fn inspect_nonpassed_identity(data: &[u8]) -> Result<Value, VerifyError> {
    let text = decode_text(data, "identity.txt")?;
    if text.starts_with("identity_attestation=unavailable\n") {
        let lines: Vec<_> = text.lines().collect();
        if lines.len() == 1 {
            return Ok(json!({"available": false, "validated": false}));
        }
        if lines.len() != 4 {
            return Err(VerifyError::new(
                "invalid_source_provenance",
                "identity unavailable 格式无效。",
            ));
        }
        let source = source_from_lines(&lines[1..])?;
        return Ok(json!({"available": false, "validated": false, "source": source}));
    }
    let mut result = json!({"available": true, "validated": false, "sha256": hex_sha256(data)});
    let lines: Vec<_> = text
        .lines()
        .take_while(|line| !line.starts_with('['))
        .filter(|line| line.starts_with("source_"))
        .collect();
    if !lines.is_empty() {
        result["source"] = source_from_lines(&lines)?;
    }
    Ok(result)
}

fn source_from_lines(lines: &[&str]) -> Result<Value, VerifyError> {
    let mut values = BTreeMap::new();
    for line in lines {
        let (key, value) = line.split_once('=').ok_or_else(|| {
            VerifyError::new(
                "invalid_source_provenance",
                "identity source provenance 无效。",
            )
        })?;
        if values.insert(key, value).is_some() {
            return Err(VerifyError::new(
                "invalid_source_provenance",
                "identity source provenance 重复。",
            ));
        }
    }
    let source = json!({
        "revision": values.get("source_revision").copied().unwrap_or(""),
        "worktree": values.get("source_worktree").copied().unwrap_or(""),
        "package_digest": values.get("source_package_digest").copied().unwrap_or(""),
    });
    if values.len() != 3 {
        return Err(VerifyError::new(
            "invalid_source_provenance",
            "identity source provenance 无效。",
        ));
    }
    validate_source(&source)?;
    Ok(source)
}

fn validate_source(source: &Value) -> Result<(), VerifyError> {
    let source = source.as_object().ok_or_else(|| {
        VerifyError::new(
            "invalid_source_provenance",
            "source provenance 必须是对象。",
        )
    })?;
    let revision = required_string(source, "revision")?;
    let worktree = required_string(source, "worktree")?;
    let digest = required_string(source, "package_digest")?;
    if !is_revision(revision)
        || !["clean", "dirty"].contains(&worktree)
        || !is_hex(digest, 64, true)
    {
        return Err(VerifyError::new(
            "invalid_source_provenance",
            "source provenance 无效。",
        ));
    }
    Ok(())
}

fn require_member<'a>(
    members: &'a BTreeMap<String, Vec<u8>>,
    name: &str,
) -> Result<&'a [u8], VerifyError> {
    members
        .get(name)
        .map(Vec::as_slice)
        .ok_or_else(|| VerifyError::new("missing_member", "归档缺少必需成员。"))
}

fn member_limit(name: &str) -> usize {
    match name {
        "report.json" => 512 * 1024,
        "README.txt" => 64 * 1024,
        "identity.txt" => 256 * 1024,
        "SHA256SUMS" => 4 * 1024,
        _ => 0,
    }
}

fn tar_name(header: &[u8]) -> Result<String, VerifyError> {
    let name = tar_text(&header[..100], "member name")?;
    let prefix = tar_text(&header[345..500], "member prefix")?;
    Ok(if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    })
}

fn tar_text(field: &[u8], label: &str) -> Result<String, VerifyError> {
    let end = field
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(field.len());
    std::str::from_utf8(&field[..end])
        .map(str::to_string)
        .map_err(|_| VerifyError::new("invalid_tar_header", format!("tar {label} 不是 UTF-8。")))
}

fn tar_octal(field: &[u8], label: &str) -> Result<u64, VerifyError> {
    let text = std::str::from_utf8(field).map_err(|_| {
        VerifyError::new("invalid_tar_header", format!("tar {label} 不是八进制数。"))
    })?;
    let stripped = text.trim_matches(|character| character == ' ' || character == '\0');
    if stripped.is_empty() {
        return Ok(0);
    }
    if !stripped.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return Err(VerifyError::new(
            "invalid_tar_header",
            format!("tar {label} 不是规范八进制数。"),
        ));
    }
    u64::from_str_radix(stripped, 8)
        .map_err(|_| VerifyError::new("invalid_tar_header", format!("tar {label} 数值溢出。")))
}

fn safe_member(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['/', '\\'])
        && !name.contains(['/', '\\'])
        && name != "."
        && name != ".."
        && !name.as_bytes().get(1).is_some_and(|byte| *byte == b':')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn decode_text<'a>(data: &'a [u8], member: &str) -> Result<&'a str, VerifyError> {
    let text = std::str::from_utf8(data).map_err(|_| {
        VerifyError::new("invalid_utf8", "文本成员不是 UTF-8。").detail("member", json!(member))
    })?;
    if text.contains('\0') {
        return Err(
            VerifyError::new("invalid_text", "文本成员包含 NUL。").detail("member", json!(member))
        );
    }
    Ok(text)
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, VerifyError> {
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && !value.contains('\n'));
    value.ok_or_else(|| invalid_report(format!("{key} 必须是非空单行字符串。")))
}

fn nonnegative(object: &Map<String, Value>, key: &str) -> Result<u64, VerifyError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_report(format!("{key} 必须是非负整数。")))
}

fn boolean(object: &Map<String, Value>, key: &str) -> Result<bool, VerifyError> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid_report(format!("{key} 必须是布尔值。")))
}

fn invalid_report(message: impl Into<String>) -> VerifyError {
    VerifyError::new("invalid_report_schema", message)
}

fn is_hex(value: &str, length: usize, lowercase: bool) -> bool {
    value.len() == length
        && value.bytes().all(|byte| {
            byte.is_ascii_digit()
                || if lowercase {
                    (b'a'..=b'f').contains(&byte)
                } else {
                    byte.is_ascii_hexdigit()
                }
        })
}

fn is_revision(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && is_hex(value, value.len(), true)
}

fn hex_sha256(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn trust_error_value(code: &str, message: &str) -> Value {
    json!({"code": code, "message": message})
}

fn timestamp_shape(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 20
        && bytes.ends_with(b"Z")
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes.get(13) == Some(&b':')
        && bytes.get(16) == Some(&b':')
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[17..19].iter().all(u8::is_ascii_digit)
        && (bytes.len() == 20
            || (bytes.len() > 21
                && bytes.get(19) == Some(&b'.')
                && bytes[20..bytes.len() - 1].iter().all(u8::is_ascii_digit)))
}

fn parse_json_no_duplicates(text: &str) -> Result<Value, VerifyError> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = NoDuplicates
        .deserialize(&mut deserializer)
        .map_err(|error| {
            if error.to_string().contains("duplicate JSON key") {
                VerifyError::new("duplicate_json_key", "report.json 包含重复 JSON key。")
            } else {
                VerifyError::new("invalid_report_json", "report.json 不是有效 JSON。")
            }
        })?;
    deserializer
        .end()
        .map_err(|_| VerifyError::new("invalid_report_json", "report.json 包含尾随数据。"))?;
    Ok(value)
}

struct NoDuplicates;

impl<'de> DeserializeSeed<'de> for NoDuplicates {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(NoDuplicatesVisitor)
    }
}

struct NoDuplicatesVisitor;

impl<'de> Visitor<'de> for NoDuplicatesVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_f64<E>(self, value: f64) -> Result<Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }
    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(json!(value))
    }
    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A>(self, mut sequence: A) -> Result<Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(NoDuplicates)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A>(self, mut map: A) -> Result<Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(A::Error::custom(format!("duplicate JSON key: {key}")));
            }
            object.insert(key, map.next_value_seed(NoDuplicates)?);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "aad-macos-verifier-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn archive(&self, files: &BTreeMap<String, Vec<u8>>) -> std::path::PathBuf {
            let tar = make_tar(files);
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&tar).unwrap();
            let deflate = encoder.finish().unwrap();
            let mut gzip = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03".to_vec();
            gzip.extend(deflate);
            gzip.extend(crc32fast::hash(&tar).to_le_bytes());
            gzip.extend((tar.len() as u32).to_le_bytes());
            let path = self.0.join("result.tar.gz");
            std::fs::write(&path, gzip).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn report(status: &str) -> Vec<u8> {
        let checks: Vec<Value> = if status == "passed" {
            REQUIRED_CHECKS
                .iter()
                .map(|id| {
                    let mut check = json!({"id": id, "status": "pass", "message": "ok"});
                    if *id == "pointer_click_and_reread" {
                        check["evidence"] = json!({
                            "fresh_target": true, "positive_area_bounds": true,
                            "bounds_ax_errors": [], "target_pid_matches_fixture": true,
                            "pid_ax_error": 0, "frontmost_before_dispatch": true,
                            "frontmost_at_dispatch": true, "status_idle_before_dispatch": true,
                            "center_derived_from_ax_bounds": true, "center_finite": true,
                            "hit_test_matches_target": true, "event_submitted": true,
                            "button": "left", "position": "center",
                            "postcondition_reread": true,
                            "status_matches_from_fresh_snapshot": true,
                            "postcondition_ax_errors": [],
                        });
                    }
                    check
                })
                .collect()
        } else {
            Vec::new()
        };
        serde_json::to_vec(&json!({
            "schema_version": "1.0", "kind": "macos_ax_fixture_test",
            "status": status, "message": "fixture result",
            "timestamp_utc": "2026-08-25T12:34:56Z",
            "source": {"revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "worktree": "clean", "package_digest": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},
            "platform": {"os": "macos", "architecture": "arm64", "version": "macOS 15.6", "rosetta_translated": false},
            "identity": {"runner_bundle_id": "dev.ai-auto-desktop.testkit.ax-runner", "fixture_bundle_id": "dev.ai-auto-desktop.testkit.fixture", "launcher_declared_identity_stability": "ephemeral"},
            "permissions": {"accessibility": {"trusted": true, "prompt_requested": false}, "screen_capture": {"preflight_granted": false, "request_attempted": false, "capture_attempted": false}},
            "limits": {"target_scope": "fixture_process_only", "screen_content_collected": false},
            "summary": {"passed": checks.len(), "failed": 0, "total": checks.len()},
            "checks": checks,
        }))
        .unwrap()
    }

    fn identity() -> Vec<u8> {
        format!(
            "swift=Apple Swift version 6.0\nidentity_stability=ephemeral\nsource_revision={}\nsource_worktree=clean\nsource_package_digest={}\n[runner]\ndesignated => identifier runner and anchor apple generic\ndesignated_requirement_origin=implicit\nIdentifier=dev.ai-auto-desktop.testkit.ax-runner\nTeamIdentifier=TESTTEAM\nCDHash=0123456789abcdef\narchitectures=arm64\nsha256={}\n[fixture]\ndesignated => identifier fixture and anchor apple generic\ndesignated_requirement_origin=implicit\nIdentifier=dev.ai-auto-desktop.testkit.fixture\nTeamIdentifier=TESTTEAM\nCDHash=89abcdef01234567\narchitectures=arm64\nsha256={}\n",
            "a".repeat(40), "b".repeat(64), "1".repeat(64), "2".repeat(64)
        )
        .into_bytes()
    }

    fn files(status: &str) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::from([
            ("report.json".to_string(), report(status)),
            ("README.txt".to_string(), b"result only\n".to_vec()),
            ("identity.txt".to_string(), identity()),
        ]);
        let manifest = HASHED_MEMBERS
            .iter()
            .map(|name| format!("{}  {name}\n", hex_sha256(&files[*name])))
            .collect::<String>();
        files.insert("SHA256SUMS".to_string(), manifest.into_bytes());
        files
    }

    fn refresh_manifest(files: &mut BTreeMap<String, Vec<u8>>) {
        let manifest = HASHED_MEMBERS
            .iter()
            .map(|name| format!("{}  {name}\n", hex_sha256(&files[*name])))
            .collect::<String>();
        files.insert("SHA256SUMS".to_string(), manifest.into_bytes());
    }

    fn make_tar(files: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
        make_tar_order(files, &EXPECTED_MEMBERS)
    }

    fn make_tar_order(files: &BTreeMap<String, Vec<u8>>, order: &[&str]) -> Vec<u8> {
        let mut tar = Vec::new();
        for name in order {
            let content = &files[*name];
            let mut header = [0u8; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            write_octal(&mut header[100..108], 0o644);
            write_octal(&mut header[108..116], 0);
            write_octal(&mut header[116..124], 0);
            write_octal(&mut header[124..136], content.len() as u64);
            write_octal(&mut header[136..148], NORMALIZED_MTIME);
            header[148..156].fill(b' ');
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[265..269].copy_from_slice(b"root");
            header[297..301].copy_from_slice(b"root");
            let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
            let checksum = format!("{checksum:06o}\0 ");
            header[148..156].copy_from_slice(checksum.as_bytes());
            tar.extend(header);
            tar.extend(content);
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.extend([0u8; 1024]);
        tar
    }

    fn write_octal(field: &mut [u8], value: u64) {
        let encoded = format!("{:0width$o}\0", value, width = field.len() - 1);
        field.copy_from_slice(encoded.as_bytes());
    }

    #[test]
    fn safe_members_are_flat_and_portable() {
        assert!(safe_member("report.json"));
        for bad in [
            "../report.json",
            "dir/report.json",
            "C:report.json",
            "",
            ".",
        ] {
            assert!(!safe_member(bad));
        }
    }

    #[test]
    fn octal_parser_rejects_non_octal_digits() {
        assert_eq!(tar_octal(b"0000644\0", "mode").unwrap(), 0o644);
        assert!(tar_octal(b"0000688\0", "mode").is_err());
    }

    #[test]
    fn passed_archive_requires_both_archive_and_source_pins() {
        let scratch = Scratch::new();
        let archive = scratch.archive(&files("passed"));
        let untrusted = verify(&archive, &VerifyOptions::default()).unwrap();
        assert_eq!(untrusted["error"]["code"], "untrusted_archive");

        let bytes = std::fs::read(&archive).unwrap();
        let qualified = verify(
            &archive,
            &VerifyOptions {
                expected_archive_sha256: Some(hex_sha256(&bytes)),
                expected_source_revision: Some("a".repeat(40)),
                expected_source_package_digest: Some("b".repeat(64)),
            },
        )
        .unwrap();
        assert_eq!(qualified["qualified"], true);
    }

    #[test]
    fn tampered_member_and_duplicate_json_fail_closed() {
        let scratch = Scratch::new();
        let mut tampered = files("passed");
        tampered
            .get_mut("README.txt")
            .unwrap()
            .extend(b"tampered\n");
        let error = verify(&scratch.archive(&tampered), &VerifyOptions::default()).unwrap_err();
        assert_eq!(error.document()["error"]["code"], "hash_mismatch");

        let mut duplicated = files("passed");
        duplicated.insert(
            "report.json".into(),
            b"{\"status\":\"passed\",\"status\":\"failed\"}\n".to_vec(),
        );
        let manifest = HASHED_MEMBERS
            .iter()
            .map(|name| format!("{}  {name}\n", hex_sha256(&duplicated[*name])))
            .collect::<String>();
        duplicated.insert("SHA256SUMS".into(), manifest.into_bytes());
        let error = verify(&scratch.archive(&duplicated), &VerifyOptions::default()).unwrap_err();
        assert_eq!(error.document()["error"]["code"], "duplicate_json_key");
    }

    #[test]
    fn missing_required_check_and_bad_evidence_fail_closed() {
        let scratch = Scratch::new();
        let mut missing = files("passed");
        let mut report: Value = serde_json::from_slice(&missing["report.json"]).unwrap();
        report["checks"].as_array_mut().unwrap().pop();
        report["summary"]["passed"] = json!(9);
        report["summary"]["total"] = json!(9);
        missing.insert("report.json".into(), serde_json::to_vec(&report).unwrap());
        refresh_manifest(&mut missing);
        let error = verify(&scratch.archive(&missing), &VerifyOptions::default()).unwrap_err();
        assert_eq!(error.document()["error"]["code"], "missing_required_checks");

        let mut bad = files("passed");
        let mut report: Value = serde_json::from_slice(&bad["report.json"]).unwrap();
        let check = report["checks"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|check| check["id"] == "pointer_click_and_reread")
            .unwrap();
        check["evidence"]["button"] = json!("right");
        bad.insert("report.json".into(), serde_json::to_vec(&report).unwrap());
        refresh_manifest(&mut bad);
        let error = verify(&scratch.archive(&bad), &VerifyOptions::default()).unwrap_err();
        assert_eq!(
            error.document()["error"]["code"],
            "invalid_pointer_evidence"
        );
    }

    #[test]
    fn archive_shape_and_metadata_are_enforced() {
        let content = files("passed");
        let tar = make_tar_order(
            &content,
            &["README.txt", "report.json", "identity.txt", "SHA256SUMS"],
        );
        let error = parse_ustar(&tar).unwrap_err();
        assert_eq!(
            error.document()["error"]["code"],
            "non_normalized_tar_order"
        );

        let mut tar = make_tar(&files("passed"));
        write_octal(&mut tar[100..108], 0o600);
        tar[148..156].fill(b' ');
        let checksum: u64 = tar[..512].iter().map(|byte| u64::from(*byte)).sum();
        tar[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        let error = parse_ustar(&tar).unwrap_err();
        assert_eq!(
            error.document()["error"]["code"],
            "non_normalized_tar_metadata"
        );
    }
}
