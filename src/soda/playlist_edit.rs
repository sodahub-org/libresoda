//! 歌单写操作（创建 / 改名 / 删除 / 加歌 / 删歌 / 排序 / 敏感词检查）。
//!
//! 上游 `music-lib` 只实现了歌单读取，这些是照官方客户端 IDL
//!（`src/idl/goapi/contract/playlist.ts` + `src/idl/goapi/main.ts`）补的写接口：
//!
//! | 能力 | 接口（PC 端） | 请求体 |
//! | --- | --- | --- |
//! | 创建 | `POST /luna/pc/me/playlist` | `{name, is_private?, track_ids?, media?}` |
//! | 改信息 | `POST /luna/pc/me/playlist/update` | `{playlist_id, name?, description?, is_private?, cover_uri?}` |
//! | 删除 | `POST /luna/pc/me/playlist/delete` | `{playlist_ids}` |
//! | 加歌 | `POST /luna/pc/me/playlist/media/append` | `{playlist_id, media[{id,type}]}` |
//! | 删歌 | `POST /luna/pc/me/playlist/media/delete` | `{playlist_id, media[{id,type}]}` |
//! | 排序 | `POST /luna/pc/me/playlist/media/sort` | `{playlist_id, media[{id,type}]}` |
//! | 敏感词 | `GET /luna/me/playlist/wordcheck` | 查询参数 `content` + `type` |
//!
//! 这些路径**不在** bdticket「零信任加签」名单里（那份名单只覆盖 collection/follow），
//! 所以只要配好应用级签名（`x-helios` / `x-medusa`）即可。

use super::media_ref::{media_array, MediaRef};
use super::Soda;
use crate::error::{Result, SodaError};

pub const CREATE_PLAYLIST_PATH: &str = "/luna/pc/me/playlist";
pub const UPDATE_PLAYLIST_PATH: &str = "/luna/pc/me/playlist/update";
pub const DELETE_PLAYLIST_PATH: &str = "/luna/pc/me/playlist/delete";
pub const APPEND_PLAYLIST_MEDIA_PATH: &str = "/luna/pc/me/playlist/media/append";
pub const DELETE_PLAYLIST_MEDIA_PATH: &str = "/luna/pc/me/playlist/media/delete";
/// 注意：排序只有非 PC 路径（`/luna/me/...`），PC 路径会 404——客户端 IDL
/// `SortPlaylistMedia` 里也只登记了这一条。
pub const SORT_PLAYLIST_MEDIA_PATH: &str = "/luna/me/playlist/media/sort";
pub const WORD_CHECK_PATH: &str = "/luna/me/playlist/wordcheck";

/// 官方 `SensitiveWordCheckType`：`name` = 歌单名，`desc` = 歌单描述。
pub const WORD_CHECK_TYPE_PLAYLIST_NAME: &str = "name";
pub const WORD_CHECK_TYPE_PLAYLIST_DESCRIPTION: &str = "desc";

fn require_cookie(soda: &Soda, what: &str) -> Result<()> {
    if !soda.has_cookie() {
        return Err(SodaError::invalid_input(format!("{what} requires cookie")));
    }
    Ok(())
}

/// 创建歌单的请求体（独立出来便于离线断言形状）。
pub fn create_playlist_body(
    name: &str,
    is_private: bool,
    track_ids: &[String],
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "name": name.trim(),
        "is_private": is_private,
    });
    if !track_ids.is_empty() {
        body["track_ids"] = serde_json::json!(track_ids
            .iter()
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>());
    }
    body
}

/// 从创建歌单的响应里取出新歌单 id（官方回包字段名做过几版，这里都兜住）。
pub fn extract_playlist_id(response: &serde_json::Value) -> String {
    for path in [
        &["data", "playlist_id"][..],
        &["data", "playlist", "id"][..],
        &["data", "id"][..],
        &["playlist", "id"][..],
        &["playlist_id"][..],
    ] {
        let mut cursor = response;
        let mut found = true;
        for key in path {
            match cursor.get(*key) {
                Some(next) => cursor = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            if let Some(text) = cursor.as_str() {
                if !text.trim().is_empty() {
                    return text.trim().to_string();
                }
            }
            if let Some(number) = cursor.as_i64() {
                return number.to_string();
            }
        }
    }
    String::new()
}

/// 创建歌单；返回新歌单 id（服务端没回 id 时为空串）。
pub fn create_playlist(
    soda: &Soda,
    name: &str,
    is_private: bool,
    track_ids: &[String],
) -> Result<String> {
    let response = create_playlist_response(soda, name, is_private, track_ids)?;
    Ok(extract_playlist_id(&response))
}

/// 创建歌单并返回原始回包（官方回包形如 `{status_code, playlist:{id,…}}`）。
pub fn create_playlist_response(
    soda: &Soda,
    name: &str,
    is_private: bool,
    track_ids: &[String],
) -> Result<serde_json::Value> {
    require_cookie(soda, "soda create playlist")?;
    if name.trim().is_empty() {
        return Err(SodaError::invalid_input(
            "soda create playlist requires name",
        ));
    }
    let body = create_playlist_body(name, is_private, track_ids);
    super::pc_post_json(soda, CREATE_PLAYLIST_PATH, &body)
}

/// 改名/改描述/改可见性/换封面（只传要改的字段）。
pub fn update_playlist_info(
    soda: &Soda,
    playlist_id: &str,
    name: Option<&str>,
    description: Option<&str>,
    is_private: Option<bool>,
    cover_uri: Option<&str>,
) -> Result<serde_json::Value> {
    require_cookie(soda, "soda update playlist")?;
    if playlist_id.trim().is_empty() {
        return Err(SodaError::invalid_input(
            "soda update playlist requires playlist_id",
        ));
    }
    let mut body = serde_json::json!({ "playlist_id": playlist_id.trim() });
    if let Some(name) = name {
        body["name"] = serde_json::json!(name.trim());
    }
    if let Some(description) = description {
        body["description"] = serde_json::json!(description.trim());
    }
    if let Some(is_private) = is_private {
        body["is_private"] = serde_json::json!(is_private);
    }
    if let Some(cover_uri) = cover_uri {
        body["cover_uri"] = serde_json::json!(cover_uri.trim());
    }
    super::pc_post_json(soda, UPDATE_PLAYLIST_PATH, &body)
}

/// 批量删除歌单。
pub fn delete_playlists(soda: &Soda, playlist_ids: &[String]) -> Result<serde_json::Value> {
    require_cookie(soda, "soda delete playlists")?;
    let ids: Vec<&str> = playlist_ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return Err(SodaError::invalid_input(
            "soda delete playlists requires playlist_ids",
        ));
    }
    let body = serde_json::json!({ "playlist_ids": ids });
    super::pc_post_json(soda, DELETE_PLAYLIST_PATH, &body)
}

fn media_body(playlist_id: &str, media: &[MediaRef]) -> Result<serde_json::Value> {
    if playlist_id.trim().is_empty() {
        return Err(SodaError::invalid_input(
            "soda playlist media requires playlist_id",
        ));
    }
    let items = media_array(media);
    if items.as_array().map(|list| list.is_empty()).unwrap_or(true) {
        return Err(SodaError::invalid_input(
            "soda playlist media requires media",
        ));
    }
    Ok(serde_json::json!({
        "playlist_id": playlist_id.trim(),
        "media": items,
    }))
}

/// 往歌单里加歌。
pub fn append_playlist_media(
    soda: &Soda,
    playlist_id: &str,
    media: &[MediaRef],
) -> Result<serde_json::Value> {
    require_cookie(soda, "soda append playlist media")?;
    let body = media_body(playlist_id, media)?;
    super::pc_post_json(soda, APPEND_PLAYLIST_MEDIA_PATH, &body)
}

/// 从歌单里删歌。
pub fn delete_playlist_media(
    soda: &Soda,
    playlist_id: &str,
    media: &[MediaRef],
) -> Result<serde_json::Value> {
    require_cookie(soda, "soda delete playlist media")?;
    let body = media_body(playlist_id, media)?;
    super::pc_post_json(soda, DELETE_PLAYLIST_MEDIA_PATH, &body)
}

/// 歌单排序：`media` 按目标顺序传入。
pub fn sort_playlist_media(
    soda: &Soda,
    playlist_id: &str,
    media: &[MediaRef],
) -> Result<serde_json::Value> {
    require_cookie(soda, "soda sort playlist media")?;
    let body = media_body(playlist_id, media)?;
    super::pc_post_json(soda, SORT_PLAYLIST_MEDIA_PATH, &body)
}

/// 敏感词检查：`kind` 取 `name` / `desc`；返回 `true` 表示**命中**敏感词。
///
/// 官方回包形如 `{status_code, is_passed}`，所以「命中」= `!is_passed`。
pub fn text_has_sensitive_word(soda: &Soda, content: &str, kind: &str) -> Result<bool> {
    require_cookie(soda, "soda word check")?;
    if content.trim().is_empty() {
        return Err(SodaError::invalid_input("soda word check requires content"));
    }
    let response = super::pc_get_json(
        soda,
        WORD_CHECK_PATH,
        &[
            ("content", content.trim().to_string()),
            ("type", kind.trim().to_string()),
        ],
    )?;

    // 先把接口级错误暴露出来，避免把 ERR_INVALID_PARAM 误判成"命中敏感词"
    let status_code = response
        .get("status_code")
        .and_then(|code| code.as_i64())
        .unwrap_or(0);
    if status_code != 0 {
        let message = response
            .get("status_info")
            .and_then(|info| info.get("status_msg"))
            .and_then(|msg| msg.as_str())
            .unwrap_or("unknown error");
        return Err(SodaError::http(format!(
            "soda word check failed: {message}（status_code={status_code}）"
        )));
    }
    for path in [&["is_passed"][..], &["data", "is_passed"][..]] {
        let mut cursor = &response;
        let mut found = true;
        for key in path {
            match cursor.get(*key) {
                Some(next) => cursor = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            if let Some(passed) = cursor.as_bool() {
                return Ok(!passed);
            }
        }
    }
    Err(SodaError::json(
        "soda word check response missing is_passed".to_string(),
    ))
}

/// 歌单名敏感词检查：`true` = 命中。
pub fn playlist_name_has_sensitive_word(soda: &Soda, name: &str) -> Result<bool> {
    text_has_sensitive_word(soda, name, WORD_CHECK_TYPE_PLAYLIST_NAME)
}

/// 歌单描述敏感词检查：`true` = 命中。
pub fn playlist_description_has_sensitive_word(soda: &Soda, description: &str) -> Result<bool> {
    text_has_sensitive_word(soda, description, WORD_CHECK_TYPE_PLAYLIST_DESCRIPTION)
}

impl Soda {
    /// 创建歌单（返回新歌单 id，服务端未回 id 时为空串）。
    pub fn create_playlist(
        &self,
        name: &str,
        is_private: bool,
        track_ids: &[String],
    ) -> Result<String> {
        create_playlist(self, name, is_private, track_ids)
    }

    /// 创建歌单并返回原始回包。
    pub fn create_playlist_response(
        &self,
        name: &str,
        is_private: bool,
        track_ids: &[String],
    ) -> Result<serde_json::Value> {
        create_playlist_response(self, name, is_private, track_ids)
    }

    /// 改名/改描述/改可见性/换封面。
    pub fn update_playlist_info(
        &self,
        playlist_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        is_private: Option<bool>,
        cover_uri: Option<&str>,
    ) -> Result<serde_json::Value> {
        update_playlist_info(self, playlist_id, name, description, is_private, cover_uri)
    }

    /// 批量删除歌单。
    pub fn delete_playlists(&self, playlist_ids: &[String]) -> Result<serde_json::Value> {
        delete_playlists(self, playlist_ids)
    }

    /// 往歌单加歌。
    pub fn append_playlist_media(
        &self,
        playlist_id: &str,
        media: &[MediaRef],
    ) -> Result<serde_json::Value> {
        append_playlist_media(self, playlist_id, media)
    }

    /// 从歌单删歌。
    pub fn delete_playlist_media(
        &self,
        playlist_id: &str,
        media: &[MediaRef],
    ) -> Result<serde_json::Value> {
        delete_playlist_media(self, playlist_id, media)
    }

    /// 歌单排序。
    pub fn sort_playlist_media(
        &self,
        playlist_id: &str,
        media: &[MediaRef],
    ) -> Result<serde_json::Value> {
        sort_playlist_media(self, playlist_id, media)
    }

    /// 歌单名敏感词检查（`true` = 命中）。
    pub fn playlist_name_has_sensitive_word(&self, name: &str) -> Result<bool> {
        playlist_name_has_sensitive_word(self, name)
    }

    /// 歌单描述敏感词检查（`true` = 命中）。
    pub fn playlist_description_has_sensitive_word(&self, description: &str) -> Result<bool> {
        playlist_description_has_sensitive_word(self, description)
    }
}
