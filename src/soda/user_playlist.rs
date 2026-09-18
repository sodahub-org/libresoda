//! 「我的歌单」（对应上游 `soda/user_playlist.go`）。

use super::playlist::build_playlist_from_user_item;
use super::types::{PCMeResponse, UserPlaylistResponse};
use super::Soda;
use crate::error::{Result, SodaError};
use crate::http;
use crate::model::Playlist;

/// 等价 `GetUserPlaylists`：先取用户 id，再分页拉取歌单并做切片。
pub fn get_user_playlists(soda: &Soda, page: i64, limit: i64) -> Result<Vec<Playlist>> {
    if !soda.has_cookie() {
        return Err(SodaError::invalid_input(
            "soda user playlists require cookie",
        ));
    }
    let page = if page < 1 { 1 } else { page };
    let limit = if limit <= 0 {
        30
    } else if limit > 100 {
        100
    } else {
        limit
    };

    let me = fetch_pc_me(soda)?;
    let user_id = me.my_info.id.trim().to_string();
    if user_id.is_empty() {
        return Err(SodaError::invalid_input(
            "soda user playlists require logged-in user id",
        ));
    }

    let target_count = page * limit;
    let request_count = target_count.clamp(50, 100);

    let mut cursor = String::new();
    let mut seen_cursors: Vec<String> = Vec::new();
    let mut seen_playlists: Vec<String> = Vec::new();
    // 防御：target_count 来自调用方的 page*limit，可能极大；
    // 每页最多 100 条、最多 20 轮，容量上限按 2048 兜底即可。
    let mut playlists: Vec<Playlist> = Vec::with_capacity(target_count.clamp(0, 2048) as usize);

    let mut attempts = 0;
    while attempts < 20 && (playlists.len() as i64) < target_count {
        attempts += 1;
        let response = fetch_user_playlist_page(soda, &user_id, &cursor, request_count)?;
        for item in &response.playlists {
            let playlist = build_playlist_from_user_item(item, &user_id, &me.my_info.nickname);
            if playlist.id.is_empty() || seen_playlists.contains(&playlist.id) {
                continue;
            }
            seen_playlists.push(playlist.id.clone());
            playlists.push(playlist);
        }

        let next_cursor = response.next_cursor.trim().to_string();
        if next_cursor.is_empty() || next_cursor == cursor || seen_cursors.contains(&next_cursor) {
            break;
        }
        if !response.has_more && (response.playlists.len() as i64) < request_count {
            break;
        }
        seen_cursors.push(next_cursor.clone());
        cursor = next_cursor;
    }

    let start = (page - 1) * limit;
    if start >= playlists.len() as i64 {
        return Ok(Vec::new());
    }
    let mut end = start + limit;
    if end > playlists.len() as i64 {
        end = playlists.len() as i64;
    }
    Ok(playlists[start as usize..end as usize].to_vec())
}

/// 等价 `fetchPCMe`。
pub fn fetch_pc_me(soda: &Soda) -> Result<PCMeResponse> {
    let body = http::get(&super::pc_me_url(), &super::pc_request_options(soda))?;
    let response: PCMeResponse = serde_json::from_slice(&body)
        .map_err(|err| SodaError::json(format!("soda me json parse error: {err}")))?;
    if response.status_code != 0 {
        return Err(SodaError::Api {
            status_code: response.status_code,
            status_msg: if response.status_info.status_msg.trim().is_empty() {
                "unknown error".to_string()
            } else {
                response.status_info.status_msg.clone()
            },
        });
    }
    Ok(response)
}

/// 等价 `fetchUserPlaylistPage`。
pub fn fetch_user_playlist_page(
    soda: &Soda,
    user_id: &str,
    cursor: &str,
    count: i64,
) -> Result<UserPlaylistResponse> {
    let url = super::pc_user_playlist_url(user_id, cursor, count);
    let body = http::get(&url, &super::pc_request_options(soda))?;
    let response: UserPlaylistResponse = serde_json::from_slice(&body)
        .map_err(|err| SodaError::json(format!("soda user playlist json parse error: {err}")))?;
    if response.status_code != 0 {
        return Err(SodaError::Api {
            status_code: response.status_code,
            status_msg: if response.status_info.status_msg.trim().is_empty() {
                "unknown error".to_string()
            } else {
                response.status_info.status_msg.clone()
            },
        });
    }
    Ok(response)
}

impl Soda {
    /// 等价 `(*Soda).GetUserPlaylists`。
    pub fn get_user_playlists(&self, page: i64, limit: i64) -> Result<Vec<Playlist>> {
        get_user_playlists(self, page, limit)
    }
}
