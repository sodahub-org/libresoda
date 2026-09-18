# 自写客户端接入指南（CLIENT-API）

> 本文面向「用 libresoda 写一个汽水音乐客户端」的场景：按功能列出要调什么、
> 怎么调、以及每一步的**实测状态**。移植对照见 [`PORTING.md`](PORTING.md)，
> 取流链路见 [`FULL-QUALITY-STREAM.md`](FULL-QUALITY-STREAM.md)。

## 0. 准备工作

```bash
# 1) 登录 Cookie：扫码登录（见 QR-LOGIN.md）或从官方客户端导出
export SODA_COOKIE='sessionid_ss=...; sid_tt=...'

# 2) 应用级签名服务（VIP 整曲必需；可自建 libmssdk，见 SIGNER-SERVICE.md）
export QISHUI_SIGNER_URL=http://<签名服务>:8899/sign
export QISHUI_SIGNER_TOKEN=<token>          # 服务端开了鉴权时必填

# 3) 设备指纹：与签名器一致（签名器开箱即用会自动生成，用 /config 取）
export SODA_DEVICE_ID=<device_id>
export SODA_IID=<iid>
export SODA_FP=<fp>
```

代码侧只需两行：

```rust
use std::sync::Arc;
use libresoda::soda::signature::HttpSignature;
use libresoda::{AppCredentials, Soda};

let soda = Soda::new(std::env::var("SODA_COOKIE").unwrap_or_default());
soda.set_app_credentials(AppCredentials {
    device_id: std::env::var("SODA_DEVICE_ID").unwrap_or_default(),
    iid: std::env::var("SODA_IID").unwrap_or_default(),
    fp: std::env::var("SODA_FP").unwrap_or_default(),
    ..Default::default()
});
if let Some(provider) = HttpSignature::from_env() {   // 读上面两个环境变量
    soda.set_signature_provider(Arc::new(provider));
}
# Ok::<(), libresoda::SodaError>(())
```

## 1. 推荐流 / 听歌模式

```rust
let mode = soda.feed_mode()?;                       // FeedModeResponse（已类型化）
for scene in mode.scenes() {                        // 拍平后的场景列表（实测 45 个）
    println!("{} scene_mode_id={} sub_queue_type={}", scene.text, scene.scene_mode_id, scene.sub_queue_type);
}

let mix = soda.discover_mix("scene_mode", 0, "", 10)?;   // DiscoverMixResponse
for block in &mix.inner_block {
    for resource in &block.resources {
        let p = &resource.entity.playlist;
        println!("{} {} {} {}", p.id, p.display_title(), p.count_tracks, p.cover_url());
    }
}
let more = mix.has_more;                            // 续播：用 next_cursor 再拉一页
```

* 原始接口：`fetch_feed_mode()` / `fetch_discover_mix(...)`（返回 `serde_json::Value`）
* 发现页首屏：`fetch_discover()`
* 实测：45 个场景（含「深夜 EMO」「粤语」「抖音漫游」）；`discover/mix` 返回 10 个 block，歌单 id/标题/曲数/封面均可解出

## 2. 我喜欢的音乐 / 收藏

```rust
use libresoda::soda::media_ref::MediaRef;

soda.collect_media(&[MediaRef::track("7501674235158431760")])?;    // 喜欢
soda.uncollect_media(&[MediaRef::track("7501674235158431760")])?;  // 取消

soda.collect_playlists(&["<playlist_id>".into()])?;
soda.collect_albums(&["<album_id>".into()])?;
soda.collect_artists(&["<artist_id>".into()])?;

// 读取「我收藏的」（单曲/歌单/专辑/艺人）
let items = soda.collected_items("", 500, &["album", "playlist"])?;  // 参数与官方客户端一致
for item in &items {
    println!("{} {} {}", item.kind(), item.id(), item.title());
}
// 我收藏的艺人（独立接口）
let artists = soda.collected_artists("", 20)?;
```

* 实测：喜欢 → 回读「我喜欢的音乐」校验 `true` → 取消；收藏歌单/专辑/艺人 → 回读 → 取消（均已还原）
* **坑**：`collected_items` 在空列表时服务端不返回 `mixed_collections` 字段（只回 `status_info`）。
  用 `collected_items`/`parse_mixed_collections` 会把这种情况当**空数组**；直接读原始 JSON 会误判成失败
* 这些路径在客户端里属「零信任加签」名单（`bdticket`），但**实测只需应用级签名即可写入**

## 3. 我创建的歌单（增删改）

```rust
let playlist_id = soda.create_playlist("我的歌单", true, &[])?;                 // 返回新歌单 id
soda.append_playlist_media(&playlist_id, &[MediaRef::track("<track_id>")])?;   // 加歌
soda.get_playlist_songs(&playlist_id)?;                                        // 回读
soda.sort_playlist_media(&playlist_id, &[/* 目标顺序 */])?;                     // 排序
soda.delete_playlist_media(&playlist_id, &[MediaRef::track("<track_id>")])?;   // 删歌
soda.update_playlist_info(&playlist_id, Some("新名字"), None, None, None)?;     // 改名
soda.delete_playlists(&[playlist_id])?;                                        // 删除
soda.playlist_name_has_sensitive_word("歌单名")?;                                // 敏感词（true=命中）
```

* 实测：创建 → 加歌 → 回读 1 首 → 排序（回读顺序确实反转）→ 删歌 → 改名 → 删除，全链路通过
* **坑**：排序只有**非 PC 路径** `/luna/me/playlist/media/sort`（PC 路径 404）
* **坑**：敏感词检查是 **GET**，`type` 取 `name`/`desc`，响应字段是 `is_passed`（非 0 `status_code` 要当错误抛）

## 4. 音质档位切换

```rust
soda.set_quality_preference("lossless");   // best/auto/空=永远选最优；还有 highest / medium / low
let info = soda.get_download_info(&song)?; // 按偏好挑选
```

| 偏好 | 语义 | 实测（同一 VIP 曲目） |
| --- | --- | --- |
| 空 / `best` / `auto` | 不限（默认，等价旧行为） | `lossless` 873kbps |
| `lossless` | 无损及以下 | `lossless` 873kbps |
| `highest` | 极高及以下 | `highest` 260kbps |
| `medium` | 标准及以下 | `higher` 132kbps |

所选档位没有候选时回退整体最优（例如用户选"标准"但该曲只有无损）。

## 5. 播放

```rust
let report = soda.check_stream_access("<track_id>")?;   // 整曲/试听诊断
let info = soda.get_download_info(&song)?;              // 取流（VIP 需签名器）
soda.download(&song, std::path::Path::new("/tmp/a.m4a"))?;  // 下载 + 解密

// 播放记录（播放器开始播放时写一条）
soda.append_recently_played_media(&[MediaRef::track("<track_id>")])?;
let recent = soda.recently_played_media("", 20, "track", "")?;
soda.delete_recently_played_media(&[MediaRef::track("<track_id>")])?;
soda.media_stats(&[("media_id", "<track_id>".into()), ("media_type", "track".into())])?;
```

* 实测：整曲无损 46.3MB/236.6s 下载并解密（ffmpeg 可解码）；最近播放 写→读→删均通过
* 播放上报 `media_stats` 带 `media_id` 返回 `stats`，不带参数只有 status

## 6. 元数据（歌词 / 封面 / 标题 / 作者 / 歌单）

```rust
let song = soda.parse("https://www.qishui.com/track/7304719759323564095")?;
println!("{} - {} / {}", song.name, song.artist, song.album);   // 标题/作者/专辑
println!("{}", song.cover);                                     // 封面
let lrc = soda.get_lyrics(&song)?;                              // 逐字歌词 → LRC
let songs = soda.get_playlist_songs("<playlist_id>")?;
```

专辑/艺人有独立详情接口（原始 JSON，含 `album_info` / `artist_info` / `hot_tracks` 等）：

```rust
let album = soda.fetch_pc_album_detail("<album_id>")?;
let artist = soda.fetch_artist_detail("<artist_id>")?;
let tracks = soda.list_artist_tracks("<artist_id>", "", 20)?;
let albums = soda.list_artist_albums("<artist_id>", "", 20)?;
```

> `Song.album_id` 可用于打开专辑详情；`Song.extra["artist_id"]` 可用于打开艺人详情
>（单曲详情和专辑详情链路已保留）。部分旧返回或游标页可能缺少艺人 ID，客户端应隐藏入口。

> 播放队列是**客户端职责**：libresoda 只提供数据与可续接的分页游标
>（`has_more` / `next_cursor`），队列本身请自己实现（官方客户端也是这么做的）。

## 7. 搜索

```rust
for song in soda.search("周杰伦")? { … }          // 单曲（Android 搜索接口）
for album in soda.search_album("周杰伦")? { … }
for artist in soda.search_artist("周杰伦")? { … } // 音乐人（含 id/头像/关注数）
for list in soda.search_playlist("华语")? { … }
let raw = soda.fetch_search_all_body("周杰伦", 1, 30)?; // /search/all 原始分组
let sug = soda.suggest("周杰")?;                  // 联想词（实测 10 条）
let hot = soda.suggest_words("default")?;         // 热搜词（实测 6 条）
```

* `search/all` 的分组顺序对齐官方搜索页：`top_results` / `playlists` /
  `artists` / `tracks` / `albums`；每个 group 自带 `display_title`、
  `display_view_all`、`has_more` / `next_cursor`
* 实测“周杰伦”：`top_results` 3 条，`playlists` / `artists` / `albums` 各 3 条，
  `tracks` 5 条；`search/artist` 可返回头像、曲目数和 `count_collected`

## 8. 常见错误与排查

| 现象 | 含义 | 处理 |
| --- | --- | --- |
| `soda xxx returned empty body` | 缺应用级签名，或设备指纹与签名器不一致 | 配 `QISHUI_SIGNER_URL`/`TOKEN`，并让 `SODA_DEVICE_ID` 与签名器一致 |
| `signer error: unauthorized（…QISHUI_SIGNER_TOKEN…）` | 签名服务要求鉴权但客户端没带 token | 设 `QISHUI_SIGNER_TOKEN`，或 `HttpSignature::with_token(..)` |
| `check_stream_access().is_preview == true` | 只拿到试听 | 签名/版权问题，见 `FULL-QUALITY-STREAM.md` |
| 列表接口"没有数据" | 部分接口空列表时**不返回字段** | 用本仓库的类型化解析（如 `parse_mixed_collections`） |
| `status_code != 0`（如 `ERR_INVALID_PARAM`） | 参数错（常见于把数组编码成 JSON 而非重复 key，或方法/路径用错） | 对照 [上游 music-lib 源码](https://github.com/guohuiyuan/music-lib) 与本文的实测结论 |

## 实测状态总表（2026-09-15）

| 功能 | 接口 | 实测 |
| --- | --- | --- |
| 推荐流 / 听歌模式 | `/luna/pc/feed/mode`、`/luna/pc/discover/mix`、`/luna/pc/discover` | ✅ 45 场景 / 10 block / 有 blocks |
| 我喜欢（收藏单曲） | `/luna/pc/me/collection/media[/delete]` | ✅ 写 + 回读校验 |
| 收藏 歌单/专辑/艺人 | `/luna/pc/me/collection/{playlist,album,artist}` | ✅ 写 + 取消 |
| 我收藏的列表 | `/luna/me/collection/mixed`（重复 key `item_types`） | ✅ 0→1→0 对照实验 |
| 我收藏的艺人 | `/luna/me/collection/artist` | ✅ 有数据 |
| 歌单写操作 | `/luna/pc/me/playlist*` + `/luna/me/playlist/media/sort` + wordcheck | ✅ 全链路 |
| 音质偏好 | `set_quality_preference` | ✅ 4 档实测 |
| 取流 / 解密 | `/luna/pc/track_v2` + CENC | ✅ 46.3MB 解密可播 |
| 最近播放 | `/luna/pc/me/recently-played-media[/delete]` | ✅ 写→读→删 |
| 播放统计 | `/luna/pc/media-stats` | ✅ 带 `media_id` 有 stats |
| 专辑 / 艺人 | `/luna/pc/albums/{id}`、`/luna/pc/artists/{id}[/tracks|/albums]` | ✅ 均有数据 |
| 搜索 / 联想 / 热搜 | `/search/{track,artist,album,playlist,all}` + `/luna/pc/sug` + `/luna/suggest-words/{type}` | ✅ 综合分组 / 音乐人 / 10 / 6 条 |
| 扫码登录 | `/passport/web/get_qrcode`、`check_qrconnect` | ✅ 真机扫码；MFA 未闭环（`QR-LOGIN.md`） |
