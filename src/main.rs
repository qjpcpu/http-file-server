use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag};
use sha1::{Digest, Sha1};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use tungstenite::handshake::derive_accept_key;

mod collaboration;
mod gallery_comments;
mod image_cache;
mod review;
mod state;
mod thumbnails;

use image_cache::{hex_digest, ImageCache};
use state::StateStore;

use collaboration::CollaborationHub;
use review::{read_review, ReviewAction, ReviewHub};

const DEFAULT_PORT: u16 = 8080;
const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;
const MAX_TEXT_VIEWER_FILE: u64 = 4 * 1024 * 1024;
const MAX_DRAWIO_VIEWER_FILE: u64 = 16 * 1024 * 1024;
const MAX_THUMBNAIL_SOURCE: u64 = 64 * 1024 * 1024;
const THUMBNAIL_MAX_EDGE: u32 = 128;
const GALLERY_THUMBNAIL_MAX_EDGE: u32 = 512;
const GALLERY_PREVIEW_MAX_EDGE: u32 = 2560;
const DRAWIO_VIEWER_PATH: &str = "/__http_file_server/drawio-viewer-31.3.1.js";
const DRAWIO_VIEWER_JS: &[u8] = include_bytes!("../assets/drawio-viewer-static-31.3.1.min.js");
const MERMAID_PATH: &str = "/__http_file_server/mermaid-11.17.2.min.js";
const MERMAID_JS: &[u8] = include_bytes!("../assets/mermaid-11.17.2.min.js");
const MARKDOWN_MERMAID_JS: &str = include_str!("../assets/markdown-mermaid.js");
const YJS_PATH: &str = "/__http_file_server/yjs-13.6.32.min.js";
const YJS_JS: &[u8] = include_bytes!("../assets/yjs-13.6.32.min.js");

#[derive(Debug, PartialEq)]
struct PortConfig {
    port: u16,
    fallback_to_random: bool,
    pid_file: Option<PathBuf>,
    web: bool,
    cache_dir: Option<PathBuf>,
    serve_dir: PathBuf,
}

#[derive(Default)]
struct RequestHeaders {
    content_length: usize,
    accept: String,
    if_none_match: String,
    fetch_dest: String,
    connection: String,
    upgrade: String,
    websocket_key: String,
    websocket_version: String,
}

fn main() {
    let port_config = match parse_args(env::args().skip(1)) {
        Ok(Some(config)) => config,
        Ok(None) => return,
        Err(message) => {
            eprintln!("错误: {message}\n\n{}", usage());
            std::process::exit(2);
        }
    };

    let root = match fs::canonicalize(&port_config.serve_dir) {
        Ok(root) if root.is_dir() => root,
        Ok(_) => {
            eprintln!("托管路径不是目录: {}", port_config.serve_dir.display());
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!(
                "无法读取托管目录 {}: {error}",
                port_config.serve_dir.display()
            );
            std::process::exit(1);
        }
    };
    let image_cache = port_config.cache_dir.as_deref().map(|path| {
        ImageCache::new(path).unwrap_or_else(|error| {
            eprintln!("无法创建图片缓存目录 {}: {error}", path.display());
            std::process::exit(1);
        })
    });
    if let Some(cache) = &image_cache {
        cache.start_maintenance();
    }
    let state = StateStore::new(port_config.cache_dir.as_deref()).unwrap_or_else(|error| {
        eprintln!("无法打开状态数据库: {error}");
        std::process::exit(1);
    });
    let listener = match bind_listener(&port_config) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("无法监听 0.0.0.0:{}: {error}", port_config.port);
            std::process::exit(1);
        }
    };
    let port = listener
        .local_addr()
        .expect("已绑定的监听器应有本地地址")
        .port();
    if let Some(path) = &port_config.pid_file {
        if let Err(error) = write_pid_file(path) {
            eprintln!("无法写入 PID 文件 {}: {error}", path.display());
            std::process::exit(1);
        }
    }

    println!("Serving {} at http://localhost:{port}", root.display());
    let collaboration = CollaborationHub::default();
    let reviews = ReviewHub::default();
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let image_cache = image_cache.clone();
                let state = state.clone();
                let root = root.clone();
                let collaboration = collaboration.clone();
                let reviews = reviews.clone();
                std::thread::spawn(move || {
                    if let Err(error) = handle_connection(
                        stream,
                        &root,
                        &collaboration,
                        &reviews,
                        port_config.web,
                        image_cache.as_ref(),
                        &state,
                    ) {
                        eprintln!("请求处理失败: {error}");
                    }
                });
            }
            Err(error) => eprintln!("连接失败: {error}"),
        }
    }
}

fn parse_args<I>(mut args: I) -> Result<Option<PortConfig>, String>
where
    I: Iterator<Item = String>,
{
    let mut port = DEFAULT_PORT;
    let mut fallback_to_random = true;
    let mut pid_file = None;
    let mut web = false;
    let mut cache_dir = None;
    let mut serve_dir = PathBuf::from(".");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-p" | "--port" => {
                let value = args.next().ok_or_else(|| format!("{arg} 后需要端口号"))?;
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("无效端口: {value}"))?;
                if port == 0 {
                    return Err("端口必须在 1 到 65535 之间".into());
                }
                fallback_to_random = false;
            }
            "-pid" | "--pid" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} 后需要 PID 文件路径"))?;
                if value.is_empty() {
                    return Err("PID 文件路径不能为空".into());
                }
                pid_file = Some(PathBuf::from(value));
            }
            "-cache" | "--cache" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} 后需要缓存根目录"))?;
                cache_dir = Some(PathBuf::from(value));
            }
            "-dir" | "--dir" => {
                let value = args.next().ok_or_else(|| format!("{arg} 后需要托管目录"))?;
                serve_dir = PathBuf::from(value);
            }
            "--web" => web = true,
            "-h" | "--help" => {
                println!("{}", usage());
                return Ok(None);
            }
            _ => return Err(format!("未知参数: {arg}")),
        }
    }
    Ok(Some(PortConfig {
        port,
        fallback_to_random,
        pid_file,
        web,
        cache_dir,
        serve_dir,
    }))
}

fn bind_listener(config: &PortConfig) -> io::Result<TcpListener> {
    match TcpListener::bind(("0.0.0.0", config.port)) {
        Err(error) if config.fallback_to_random && error.kind() == io::ErrorKind::AddrInUse => {
            TcpListener::bind(("0.0.0.0", 0))
        }
        result => result,
    }
}

fn usage() -> &'static str {
    "用法: http [-p PORT] [-pid FILE] [-cache DIR] [-dir DIR] [--web]\n\n选项:\n  -p, --port PORT    指定监听端口（默认 8080）\n  -pid, --pid FILE   将启动进程 PID 写入指定文件\n  -cache, --cache DIR 指定缓存根目录（缩略图、点赞和待删除标记存入 DIR）\n  -dir, --dir DIR    指定托管目录（默认当前目录）\n  --web             以原始静态网站服务器模式运行\n  -h, --help         显示帮助"
}

fn write_pid_file(path: &Path) -> io::Result<()> {
    fs::write(path, format!("{}\n", std::process::id()))
}

fn handle_connection(
    mut stream: TcpStream,
    root: &Path,
    collaboration: &CollaborationHub,
    reviews: &ReviewHub,
    web: bool,
    image_cache: Option<&ImageCache>,
    state: &StateStore,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let head_only = method == "HEAD";

    if (web && !matches!(method, "GET" | "HEAD"))
        || (!web && !matches!(method, "GET" | "HEAD" | "POST" | "PUT" | "DELETE"))
    {
        return send_text(
            &mut stream,
            405,
            "Method Not Allowed",
            if web {
                "仅支持 GET 和 HEAD\n"
            } else {
                "仅支持 GET、HEAD、POST、PUT 和 DELETE\n"
            },
            head_only,
        );
    }

    let headers = match read_request_headers(&mut reader) {
        Ok(headers) if headers.content_length <= MAX_REQUEST_BODY => headers,
        Ok(_) => {
            return send_text(
                &mut stream,
                413,
                "Content Too Large",
                "请求内容不能超过 16 MB\n",
                head_only,
            )
        }
        Err(_) => return send_text(&mut stream, 400, "Bad Request", "无效的请求头\n", head_only),
    };

    let (request_path, query) = target.split_once('?').unwrap_or((target, ""));
    let request_path = request_path.split('#').next().unwrap_or("/");
    if web {
        return handle_web_request(&mut stream, root, request_path, query, head_only);
    }
    if matches!(method, "GET" | "HEAD") && request_path == DRAWIO_VIEWER_PATH {
        return send_content(
            &mut stream,
            DRAWIO_VIEWER_JS,
            "text/javascript; charset=utf-8",
            head_only,
        );
    }
    if matches!(method, "GET" | "HEAD") && request_path == MERMAID_PATH {
        return send_content(
            &mut stream,
            MERMAID_JS,
            "text/javascript; charset=utf-8",
            head_only,
        );
    }
    if matches!(method, "GET" | "HEAD") && request_path == YJS_PATH {
        return send_content(
            &mut stream,
            YJS_JS,
            "text/javascript; charset=utf-8",
            head_only,
        );
    }
    if matches!(method, "GET" | "HEAD")
        && matches!(request_path, "/favicon.svg" | "/favicon.ico")
        && !root.join(request_path.trim_start_matches('/')).is_file()
    {
        let icon = render_site_icon(root);
        return send_content(&mut stream, icon.as_bytes(), "image/svg+xml", head_only);
    }
    let mode = query_parameter(query, "mode");
    let requested_image_version = query_parameter(query, "v");
    let raw_mode = mode.as_deref() == Some("raw");
    let preview_mode = mode.as_deref() == Some("preview");
    let asset_mode = mode.as_deref() == Some("asset");
    let thumb_mode = mode.as_deref() == Some("thumb");
    let gallery_thumb_mode = mode.as_deref() == Some("gallery-thumb");
    let gallery_preview_mode = mode.as_deref() == Some("gallery-preview");
    let collaboration_mode = mode.as_deref() == Some("collab");
    let review_data_mode = mode.as_deref() == Some("review-data");
    let review_action_mode = mode.as_deref() == Some("review-action");
    let review_collaboration_mode = mode.as_deref() == Some("review-collab");
    let gallery_comments_mode = mode.as_deref() == Some("gallery-comments");
    let decoded = match percent_decode(request_path) {
        Some(path) => path,
        None => return send_text(&mut stream, 400, "Bad Request", "无效的 URL\n", head_only),
    };
    let relative = match safe_relative_path(&decoded) {
        Some(path) => path,
        None => return send_text(&mut stream, 403, "Forbidden", "禁止访问\n", head_only),
    };

    let canonical = match fs::canonicalize(root.join(&relative)) {
        Ok(path) if path.starts_with(root) => path,
        Ok(_) => return send_text(&mut stream, 403, "Forbidden", "禁止访问\n", head_only),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let body = render_not_found_page(&decoded);
            return send_html_status(&mut stream, 404, "Not Found", &body, head_only);
        }
        Err(error) => return Err(error),
    };

    let metadata = fs::metadata(&canonical)?;
    if mode.as_deref() == Some("move-favourites") {
        if method != "POST" || !metadata.is_dir() {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "请从目录页整理图片\n",
                head_only,
            );
        }
        return match move_favourite_images(&canonical, state) {
            Ok(result) => send_content(
                &mut stream,
                &serde_json::to_vec(&result)?,
                "application/json; charset=utf-8",
                false,
            ),
            Err(error) => send_text(
                &mut stream,
                500,
                "Internal Server Error",
                &format!("整理图片失败：{error}"),
                false,
            ),
        };
    }
    if mode.as_deref() == Some("favourite") {
        if !matches!(method, "PUT" | "DELETE") || !metadata.is_file() || !is_image_file(&canonical)
        {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "仅支持点赞或取消点赞图片\n",
                head_only,
            );
        }
        return match state.set_favourite(&canonical, method == "PUT") {
            Ok(()) => send_empty(&mut stream, 204, "No Content"),
            Err(_) => send_text(
                &mut stream,
                500,
                "Internal Server Error",
                "保存点赞状态失败，请重试。\n",
                false,
            ),
        };
    }
    if mode.as_deref() == Some("deletion-mark") {
        if !matches!(method, "PUT" | "DELETE") || !metadata.is_file() || !is_image_file(&canonical)
        {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "仅支持标记或取消标记待删除图片\n",
                head_only,
            );
        }
        return match state.set_deletion_mark(&canonical, method == "PUT") {
            Ok(()) => send_empty(&mut stream, 204, "No Content"),
            Err(_) => send_text(
                &mut stream,
                500,
                "Internal Server Error",
                "保存待删除标记失败，请重试。\n",
                false,
            ),
        };
    }
    if mode.as_deref() == Some("delete-marked") {
        if method != "POST" || !metadata.is_dir() {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "请从目录页删除已标记图片\n",
                head_only,
            );
        }
        return match delete_marked_images(&canonical, state) {
            Ok(result) => send_json(&mut stream, &result, false),
            Err(error) => send_text(
                &mut stream,
                500,
                "Internal Server Error",
                &format!("删除已标记图片失败：{error}"),
                false,
            ),
        };
    }
    if mode.as_deref() == Some("clear-deletion-marks") {
        if method != "POST" || !metadata.is_dir() {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "请从目录页取消待删除标记\n",
                head_only,
            );
        }
        return match clear_directory_deletion_marks(&canonical, state) {
            Ok(result) => send_json(&mut stream, &result, false),
            Err(error) => send_text(
                &mut stream,
                500,
                "Internal Server Error",
                &format!("取消待删除标记失败：{error}"),
                false,
            ),
        };
    }
    if method == "DELETE" {
        if !metadata.is_file() || !is_image_file(&canonical) {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "仅支持删除图片文件\n",
                false,
            );
        }
        return match fs::remove_file(root.join(&relative)) {
            Ok(()) => match state.clear_image_state(&canonical) {
                Ok(()) => send_empty(&mut stream, 204, "No Content"),
                Err(_) => send_text(
                    &mut stream,
                    500,
                    "Internal Server Error",
                    "图片已删除，但清除待删除标记失败。\n",
                    false,
                ),
            },
            Err(_) => send_text(
                &mut stream,
                500,
                "Internal Server Error",
                "删除失败，请检查文件是否可写后重试。\n",
                false,
            ),
        };
    }
    if metadata.is_dir() {
        if !request_path.ends_with('/') {
            let location = if query.is_empty() {
                format!("{request_path}/")
            } else {
                format!("{request_path}/?{query}")
            };
            return send_redirect(&mut stream, &location);
        }
        let body = render_directory_page(root, &canonical, state)?;
        return send_html(&mut stream, &body, head_only);
    }
    if !metadata.is_file() {
        let body = render_not_found_page(&decoded);
        return send_html_status(&mut stream, 404, "Not Found", &body, head_only);
    }

    if gallery_comments_mode {
        if !is_image_file(&canonical) {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "仅支持图片评论\n",
                head_only,
            );
        }
        if matches!(method, "GET" | "HEAD") {
            return match gallery_comments::read(&canonical) {
                Ok(comments) => send_json(&mut stream, &comments, head_only),
                Err(error) => send_text(
                    &mut stream,
                    500,
                    "Internal Server Error",
                    &format!("读取图片评论失败：{error}\n"),
                    head_only,
                ),
            };
        }
        if method == "POST" {
            let body = match read_request_body(&mut reader, headers.content_length) {
                Ok(body) => body,
                Err(error) => return send_text(&mut stream, 400, "Bad Request", &error, false),
            };
            let action = match serde_json::from_str::<gallery_comments::GalleryCommentAction>(&body)
            {
                Ok(action) => action,
                Err(error) => {
                    return send_text(
                        &mut stream,
                        400,
                        "Bad Request",
                        &format!("无效的图片评论操作：{error}\n"),
                        false,
                    )
                }
            };
            return match gallery_comments::apply(&canonical, action) {
                Ok(comments) => send_json(&mut stream, &comments, false),
                Err(error) => send_text(&mut stream, 409, "Conflict", &format!("{error}\n"), false),
            };
        }
        return send_text(
            &mut stream,
            405,
            "Method Not Allowed",
            "仅支持读取或更新图片评论\n",
            head_only,
        );
    }

    if review_collaboration_mode {
        if method != "GET" || !has_extension(&canonical, "md") {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "仅支持协同审阅 Markdown 文件\n",
                head_only,
            );
        }
        if !is_websocket_upgrade(&headers) {
            return send_text(
                &mut stream,
                426,
                "Upgrade Required",
                "该接口需要 WebSocket 连接\n",
                false,
            );
        }
        let connection = reviews.connect(&canonical);
        let partially_read = reader.buffer().to_vec();
        let accept = derive_accept_key(headers.websocket_key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )?;
        drop(reader);
        return connection.run(stream, partially_read, MAX_REQUEST_BODY);
    }

    if review_data_mode {
        if !matches!(method, "GET" | "HEAD") || !has_extension(&canonical, "md") {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "仅支持读取 Markdown 审阅数据\n",
                head_only,
            );
        }
        return match read_review(&canonical) {
            Ok(review) => send_json(&mut stream, &review, head_only),
            Err(error) => send_text(
                &mut stream,
                500,
                "Internal Server Error",
                &format!("{error}\n"),
                head_only,
            ),
        };
    }

    if collaboration_mode {
        if method != "GET" || !has_extension(&canonical, "md") {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "仅支持协同编辑 Markdown 文件\n",
                head_only,
            );
        }
        if !is_websocket_upgrade(&headers) {
            return send_text(
                &mut stream,
                426,
                "Upgrade Required",
                "该接口需要 WebSocket 连接\n",
                false,
            );
        }
        if metadata.len() > MAX_REQUEST_BODY as u64 {
            return send_text(
                &mut stream,
                413,
                "Content Too Large",
                "协同编辑的 Markdown 文件不能超过 16 MB\n",
                false,
            );
        }
        let markdown = match fs::read_to_string(&canonical) {
            Ok(markdown) => markdown,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return send_text(
                    &mut stream,
                    415,
                    "Unsupported Media Type",
                    "Markdown 必须是 UTF-8 文本\n",
                    false,
                )
            }
            Err(error) => return Err(error),
        };
        let connection = collaboration.connect(&canonical, &markdown)?;
        let partially_read = reader.buffer().to_vec();
        let accept = derive_accept_key(headers.websocket_key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )?;
        drop(reader);
        return connection.run(
            stream,
            partially_read,
            MAX_REQUEST_BODY * 2,
            MAX_REQUEST_BODY,
        );
    }

    if method == "POST" {
        if review_action_mode && has_extension(&canonical, "md") {
            let body = match read_request_body(&mut reader, headers.content_length) {
                Ok(body) => body,
                Err(error) => return send_text(&mut stream, 400, "Bad Request", &error, false),
            };
            let action = match serde_json::from_str::<ReviewAction>(&body) {
                Ok(action) => action,
                Err(error) => {
                    return send_text(
                        &mut stream,
                        400,
                        "Bad Request",
                        &format!("无效的审阅操作：{error}\n"),
                        false,
                    )
                }
            };
            return match reviews.apply_action(&canonical, action) {
                Ok(review) => send_json(&mut stream, &review, false),
                Err(error) => send_text(&mut stream, 409, "Conflict", &format!("{error}\n"), false),
            };
        }
        if !preview_mode || !has_extension(&canonical, "md") {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "该资源不支持预览\n",
                false,
            );
        }
        let markdown = match read_request_body(&mut reader, headers.content_length) {
            Ok(body) => body,
            Err(error) => {
                return send_text(&mut stream, 400, "Bad Request", &error, false);
            }
        };
        let body = render_markdown_preview_page(&markdown);
        return send_html(&mut stream, &body, false);
    }

    if method == "PUT" {
        if !raw_mode || !has_extension(&canonical, "md") {
            return send_text(
                &mut stream,
                405,
                "Method Not Allowed",
                "仅支持保存 Markdown 文件\n",
                false,
            );
        }
        let markdown = match read_request_body(&mut reader, headers.content_length) {
            Ok(body) => body,
            Err(error) => {
                return send_text(&mut stream, 400, "Bad Request", &error, false);
            }
        };
        if !collaboration.replace_if_active(&canonical, &markdown)? {
            fs::write(&canonical, markdown.as_bytes())?;
        }
        return send_empty(&mut stream, 204, "No Content");
    }

    // Image previews load the original asset through this internal URL so SVG
    // markup is never injected into the viewer document.
    if asset_mode && is_image_file(&canonical) {
        return send_image_asset(
            &mut stream,
            &canonical,
            &metadata,
            head_only,
            image_cache.is_some(),
            requested_image_version.as_deref(),
            &headers.if_none_match,
        );
    }
    if asset_mode {
        return send_file(
            &mut stream,
            &canonical,
            metadata.len(),
            mime_type(&canonical),
            head_only,
        );
    }

    // Directory listings request a downscaled preview instead of decoding a
    // full-size photo into a 40px well.
    if thumb_mode || gallery_thumb_mode || gallery_preview_mode {
        let max_edge = if gallery_preview_mode {
            GALLERY_PREVIEW_MAX_EDGE
        } else if gallery_thumb_mode {
            GALLERY_THUMBNAIL_MAX_EDGE
        } else {
            THUMBNAIL_MAX_EDGE
        };
        return send_image_thumbnail(
            &mut stream,
            &canonical,
            &metadata,
            max_edge,
            head_only,
            image_cache,
            requested_image_version.as_deref(),
            &headers.if_none_match,
        );
    }

    if raw_mode {
        return send_file(
            &mut stream,
            &canonical,
            metadata.len(),
            "text/plain; charset=utf-8",
            head_only,
        );
    }

    if has_extension(&canonical, "html") || has_extension(&canonical, "htm") {
        return send_file(
            &mut stream,
            &canonical,
            metadata.len(),
            mime_type(&canonical),
            head_only,
        );
    }

    if has_extension(&canonical, "svg") && request_wants_html(&headers) {
        let title = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("SVG image");
        let body = render_svg_page(title, metadata.len());
        return send_file_page(&mut stream, &body, &relative, head_only);
    }

    if is_raster_image(&canonical) && request_wants_html(&headers) {
        let title = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Image");
        let kind = canonical
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("IMAGE")
            .to_ascii_uppercase();
        let body = render_image_page(title, &kind, metadata.len());
        return send_file_page(&mut stream, &body, &relative, head_only);
    }

    if has_extension(&canonical, "drawio") && request_wants_html(&headers) {
        if metadata.len() > MAX_DRAWIO_VIEWER_FILE {
            return send_text(
                &mut stream,
                413,
                "Content Too Large",
                "Draw.io 文件超过 16 MB，请使用 ?mode=raw 查看源码\n",
                head_only,
            );
        }
        let diagram = match fs::read_to_string(&canonical) {
            Ok(diagram) => diagram,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return send_text(
                    &mut stream,
                    415,
                    "Unsupported Media Type",
                    "Draw.io 文件必须是 UTF-8 XML\n",
                    head_only,
                )
            }
            Err(error) => return Err(error),
        };
        let title = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Draw.io diagram");
        let body = render_drawio_page(&diagram, title, metadata.len());
        return send_file_page(&mut stream, &body, &relative, head_only);
    }

    if has_extension(&canonical, "md") {
        let markdown = fs::read_to_string(&canonical)?;
        let title = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Markdown");
        let body = render_markdown_page(&markdown, title);
        return send_file_page(&mut stream, &body, &relative, head_only);
    }

    if request_wants_html(&headers) && metadata.len() <= MAX_TEXT_VIEWER_FILE {
        if let Some(text_file) = read_text_file(&canonical, metadata.len())? {
            let body = render_text_page(
                &text_file.content,
                &text_file.title,
                text_file.kind,
                &canonical,
            );
            return send_file_page(&mut stream, &body, &relative, head_only);
        }
    }

    send_file(
        &mut stream,
        &canonical,
        metadata.len(),
        mime_type(&canonical),
        head_only,
    )
}

fn handle_web_request(
    stream: &mut TcpStream,
    root: &Path,
    request_path: &str,
    query: &str,
    head_only: bool,
) -> io::Result<()> {
    let decoded = match percent_decode(request_path) {
        Some(path) => path,
        None => return send_text(stream, 400, "Bad Request", "无效的 URL\n", head_only),
    };
    let relative = match safe_relative_path(&decoded) {
        Some(path) => path,
        None => return send_text(stream, 403, "Forbidden", "禁止访问\n", head_only),
    };
    let canonical = match fs::canonicalize(root.join(relative)) {
        Ok(path) if path.starts_with(root) => path,
        Ok(_) => return send_text(stream, 403, "Forbidden", "禁止访问\n", head_only),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return send_text(stream, 404, "Not Found", "Not Found\n", head_only)
        }
        Err(error) => return Err(error),
    };

    let metadata = fs::metadata(&canonical)?;
    if metadata.is_dir() {
        if !request_path.ends_with('/') {
            let location = if query.is_empty() {
                format!("{request_path}/")
            } else {
                format!("{request_path}/?{query}")
            };
            return send_redirect(stream, &location);
        }
        let index = match fs::canonicalize(canonical.join("index.html")) {
            Ok(path) if path.starts_with(root) => path,
            Ok(_) => return send_text(stream, 403, "Forbidden", "Forbidden\n", head_only),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return send_text(stream, 403, "Forbidden", "Forbidden\n", head_only)
            }
            Err(error) => return Err(error),
        };
        let index_metadata = fs::metadata(&index)?;
        if !index_metadata.is_file() {
            return send_text(stream, 403, "Forbidden", "Forbidden\n", head_only);
        }
        return send_file(
            stream,
            &index,
            index_metadata.len(),
            mime_type(&index),
            head_only,
        );
    }
    if !metadata.is_file() {
        return send_text(stream, 404, "Not Found", "Not Found\n", head_only);
    }
    send_file(
        stream,
        &canonical,
        metadata.len(),
        mime_type(&canonical),
        head_only,
    )
}

fn send_file(
    stream: &mut TcpStream,
    path: &Path,
    length: u64,
    content_type: &str,
    head_only: bool,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
        length, content_type
    )?;
    if !head_only {
        let mut file = File::open(path)?;
        io::copy(&mut file, stream)?;
    }
    Ok(())
}

fn send_image_headers(
    stream: &mut TcpStream,
    length: u64,
    content_type: &str,
    etag: Option<&str>,
    if_none_match: &str,
    immutable: bool,
) -> io::Result<bool> {
    let cache_control = if immutable {
        "private, max-age=31536000, immutable"
    } else if etag.is_some() {
        "no-cache"
    } else {
        "no-store"
    };
    if let Some(etag) = etag {
        let unchanged = if_none_match.split(',').any(|value| {
            let value = value.trim();
            value == "*" || value.strip_prefix("W/").unwrap_or(value) == etag
        });
        if unchanged {
            write!(stream, "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nCache-Control: {cache_control}\r\nConnection: close\r\n\r\n")?;
            return Ok(false);
        }
    }
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nContent-Type: {content_type}\r\nConnection: close\r\n")?;
    if let Some(etag) = etag {
        write!(
            stream,
            "ETag: {etag}\r\nCache-Control: {cache_control}\r\n\r\n"
        )?;
    } else {
        write!(stream, "Cache-Control: {cache_control}\r\n\r\n")?;
    }
    Ok(true)
}

fn send_image_asset(
    stream: &mut TcpStream,
    path: &Path,
    metadata: &fs::Metadata,
    head_only: bool,
    cache_enabled: bool,
    requested_version: Option<&str>,
    if_none_match: &str,
) -> io::Result<()> {
    let version = file_version(metadata);
    let etag = cache_enabled.then(|| format!("\"{version}\""));
    let immutable = cache_enabled && requested_version == Some(version.as_str());
    let modified = send_image_headers(
        stream,
        metadata.len(),
        mime_type(path),
        etag.as_deref(),
        if_none_match,
        immutable,
    )?;
    if modified && !head_only {
        let mut file = File::open(path)?;
        io::copy(&mut file, stream)?;
    }
    Ok(())
}

fn read_request_headers<R: BufRead>(reader: &mut R) -> io::Result<RequestHeaders> {
    let mut headers = RequestHeaders::default();
    let mut total = 0;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        total += bytes;
        if total > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers are too large",
            ));
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                headers.content_length = value.trim().parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid content-length")
                })?;
            } else if name.eq_ignore_ascii_case("if-none-match") {
                headers.if_none_match = value.trim().to_owned();
            } else if name.eq_ignore_ascii_case("accept") {
                headers.accept = value.trim().to_ascii_lowercase();
            } else if name.eq_ignore_ascii_case("sec-fetch-dest") {
                headers.fetch_dest = value.trim().to_ascii_lowercase();
            } else if name.eq_ignore_ascii_case("connection") {
                headers.connection = value.trim().to_ascii_lowercase();
            } else if name.eq_ignore_ascii_case("upgrade") {
                headers.upgrade = value.trim().to_ascii_lowercase();
            } else if name.eq_ignore_ascii_case("sec-websocket-key") {
                headers.websocket_key = value.trim().to_string();
            } else if name.eq_ignore_ascii_case("sec-websocket-version") {
                headers.websocket_version = value.trim().to_string();
            }
        }
    }
    Ok(headers)
}

fn is_websocket_upgrade(headers: &RequestHeaders) -> bool {
    let key = headers.websocket_key.as_bytes();
    headers.upgrade == "websocket"
        && headers
            .connection
            .split(',')
            .any(|value| value.trim() == "upgrade")
        && headers.websocket_version == "13"
        && key.len() == 24
        && key[22..] == *b"=="
        && key[..22]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
}

fn request_wants_html(headers: &RequestHeaders) -> bool {
    headers.fetch_dest == "document" || headers.accept.contains("text/html")
}

fn read_request_body<R: Read>(reader: &mut R, length: usize) -> Result<String, String> {
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|_| "请求内容不完整\n".to_string())?;
    String::from_utf8(body).map_err(|_| "Markdown 必须是 UTF-8 文本\n".to_string())
}

fn send_empty(stream: &mut TcpStream, status: u16, reason: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

const FILE_SHORTCUT_JS: &str = include_str!("../assets/file-shortcuts.js");

const FILE_NAVIGATION_CSS: &str = r#"
body { padding-top:2.4rem; }
body:has(> .topbar) { padding-top:calc(5.8rem + 1px); }
body > header,.topbar { top:2.4rem; }
.file-breadcrumbs { position:fixed; inset:0 0 auto; z-index:8; display:flex; align-items:center; gap:.55rem; height:2.4rem; padding:0 var(--file-header-padding); color:var(--muted); background:var(--paper); border-bottom:1px solid var(--line); font:600 .78rem/1.4 ui-monospace,SFMono-Regular,Consolas,monospace; scrollbar-width:none; }
.file-breadcrumbs > a,.file-breadcrumbs > span { flex-shrink:0; }
.file-breadcrumbs a { color:inherit; text-decoration:none; white-space:nowrap; }
.file-breadcrumbs > a:last-child { flex-shrink:1; min-width:0; overflow:hidden; text-overflow:ellipsis; }
.file-breadcrumbs.measuring > a:last-child { flex-shrink:0; }
.file-breadcrumbs [hidden] { display:none !important; }
.file-breadcrumbs a:hover { color:var(--accent); }
#history-back { flex:0 0 2rem; padding:0; border:0; background:transparent; font:inherit; cursor:pointer; }
#history-back:hover { color:var(--accent); background:var(--accent-soft); }
.breadcrumb-overflow { display:flex; align-items:center; gap:.55rem; }
.breadcrumb-overflow details { position:relative; }
.breadcrumb-overflow summary { padding:.1rem .35rem; border-radius:.3rem; color:var(--accent); cursor:pointer; list-style:none; }
.breadcrumb-overflow summary::-webkit-details-marker { display:none; }
.breadcrumb-overflow summary:hover,.breadcrumb-overflow details[open] summary { background:var(--accent-soft); }
.breadcrumb-menu { position:absolute; top:calc(100% + .4rem); left:0; display:grid; min-width:8rem; width:max-content; max-width:min(24rem,calc(100vw - 8rem)); max-height:60vh; overflow:auto; padding:.35rem; border:1px solid var(--line); border-radius:.5rem; background:var(--surface); box-shadow:0 8px 24px rgba(0,0,0,.12); }
.breadcrumb-menu a { overflow:hidden; padding:.5rem .65rem; text-overflow:ellipsis; border-radius:.3rem; }
.breadcrumb-menu a:hover { background:var(--accent-soft); }
"#;

fn send_file_page(
    stream: &mut TcpStream,
    body: &str,
    relative: &Path,
    head_only: bool,
) -> io::Result<()> {
    let body = body
        .replacen(
            "<body",
            &format!(
                "<body data-file-path=\"{}\"",
                escape_html(&relative.to_string_lossy())
            ),
            1,
        )
        .replacen(
            "</head>",
            &format!("<style>{FILE_NAVIGATION_CSS}</style></head>"),
            1,
        )
        .replacen(
            "<header",
            &format!(
                "<nav class=\"file-breadcrumbs\" aria-label=\"当前位置\">{}</nav><header",
                render_breadcrumbs(relative.parent().unwrap())
            ),
            1,
        )
        .replacen(
            "</body>",
            &format!("<script>{FILE_SHORTCUT_JS}</script></body>"),
            1,
        );
    send_html(stream, &body, head_only)
}

fn send_html(stream: &mut TcpStream, body: &str, head_only: bool) -> io::Result<()> {
    send_content(
        stream,
        body.as_bytes(),
        "text/html; charset=utf-8",
        head_only,
    )
}

fn send_html_status(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
    head_only: bool,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n",
        body.len(),
    )?;
    if !head_only {
        stream.write_all(body.as_bytes())?;
    }
    Ok(())
}

fn send_content(
    stream: &mut TcpStream,
    body: &[u8],
    content_type: &str,
    head_only: bool,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",
        body.len(),
    )?;
    if !head_only {
        stream.write_all(body)?;
    }
    Ok(())
}

fn send_json<T: serde::Serialize>(
    stream: &mut TcpStream,
    value: &T,
    head_only: bool,
) -> io::Result<()> {
    let body = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    send_content(stream, &body, "application/json; charset=utf-8", head_only)
}

fn send_redirect(stream: &mut TcpStream, location: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

#[derive(Debug)]
struct DirectoryEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    image_version: Option<String>,
    favourite: bool,
    deletion_marked: bool,
}

#[derive(Default, serde::Serialize)]
struct MoveImagesResult {
    moved: usize,
    errors: Vec<String>,
}

fn available_destination_path(directory: &Path, file_name: &OsStr) -> io::Result<PathBuf> {
    let target = directory.join(file_name);
    match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(target),
        Ok(_) => {}
        Err(error) => return Err(error),
    }

    let file = Path::new(file_name);
    let stem = file.file_stem().unwrap_or(file_name);
    for index in 2.. {
        let mut renamed = OsString::from(stem);
        renamed.push(format!("_{index}"));
        if let Some(extension) = file.extension() {
            renamed.push(".");
            renamed.push(extension);
        }
        let candidate = directory.join(renamed);
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => {}
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

fn move_favourite_images(directory: &Path, state: &StateStore) -> io::Result<MoveImagesResult> {
    let mut result = MoveImagesResult::default();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let source = entry.path();
        if !source.is_file() || !is_image_file(&source) {
            continue;
        }
        let outcome = (|| -> io::Result<()> {
            let source_key = fs::canonicalize(&source)?;
            if !state.is_favourite(&source_key)? {
                return Ok(());
            }
            let deletion_marked = state.is_deletion_marked(&source_key)?;
            let target_directory = directory.join("favourites");
            fs::create_dir_all(&target_directory)?;
            let target_directory = fs::canonicalize(target_directory)?;
            let target = available_destination_path(&target_directory, &entry.file_name())?;
            fs::rename(&source, &target)?;
            result.moved += 1;
            state.set_favourite(&target, true)?;
            state.set_favourite(&source_key, false)?;
            if deletion_marked {
                state.set_deletion_mark(&target, true)?;
                state.set_deletion_mark(&source_key, false)?;
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            result
                .errors
                .push(format!("{}：{error}", entry.file_name().to_string_lossy()));
        }
    }
    Ok(result)
}

#[derive(Default, serde::Serialize)]
struct DeleteMarkedImagesResult {
    deleted: usize,
    errors: Vec<String>,
}

#[derive(Default, serde::Serialize)]
struct ClearDeletionMarksResult {
    cleared: usize,
    errors: Vec<String>,
}

fn clear_directory_deletion_marks(
    directory: &Path,
    state: &StateStore,
) -> io::Result<ClearDeletionMarksResult> {
    let mut result = ClearDeletionMarksResult::default();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || !is_image_file(&path) {
            continue;
        }
        let canonical = fs::canonicalize(&path)?;
        if !state.is_deletion_marked(&canonical)? {
            continue;
        }
        match state.set_deletion_mark(&canonical, false) {
            Ok(()) => result.cleared += 1,
            Err(error) => result
                .errors
                .push(format!("{}：{error}", entry.file_name().to_string_lossy())),
        }
    }
    Ok(result)
}

fn delete_marked_images(
    directory: &Path,
    state: &StateStore,
) -> io::Result<DeleteMarkedImagesResult> {
    let mut result = DeleteMarkedImagesResult::default();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || !is_image_file(&path) {
            continue;
        }
        let canonical = match fs::canonicalize(&path) {
            Ok(path) => path,
            Err(error) => {
                result
                    .errors
                    .push(format!("{}：{error}", entry.file_name().to_string_lossy()));
                continue;
            }
        };
        if !state.is_deletion_marked(&canonical)? {
            continue;
        }
        if let Err(error) = fs::remove_file(&path) {
            result
                .errors
                .push(format!("{}：{error}", entry.file_name().to_string_lossy()));
            continue;
        }
        result.deleted += 1;
        if let Err(error) = state.clear_image_state(&canonical) {
            result.errors.push(format!(
                "{} 已删除，但清理缓存失败：{error}",
                entry.file_name().to_string_lossy()
            ));
        }
    }
    Ok(result)
}

fn render_site_icon(root: &Path) -> String {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("HTTP");
    let initial = name
        .chars()
        .find(|character| !character.is_whitespace())
        .map(|character| character.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "H".to_string());
    let hash = name.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    let colors = [
        "#5B5BD6", "#287D8E", "#A24A70", "#3E6F49", "#8A5A2B", "#5367A8",
    ];
    let color = colors[hash as usize % colors.len()];
    let initial = escape_html(&initial);
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 64 64\"><rect width=\"64\" height=\"64\" rx=\"15\" fill=\"{color}\"/><circle cx=\"51\" cy=\"13\" r=\"5\" fill=\"#fff\" opacity=\".22\"/><text x=\"32\" y=\"42\" text-anchor=\"middle\" fill=\"#fff\" font-family=\"ui-sans-serif,system-ui,sans-serif\" font-size=\"32\" font-weight=\"750\">{initial}</text></svg>"
    )
}

fn render_not_found_page(request_path: &str) -> String {
    let path = escape_html(if request_path.is_empty() {
        "/"
    } else {
        request_path
    });
    format!(
        "<!doctype html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\n<title>文件未找到 · 404</title>\n<style>{NOT_FOUND_CSS}</style>\n</head>\n<body>\n<main><section class=\"message\" aria-labelledby=\"not-found-title\"><p class=\"status\"><span></span>HTTP 404</p><h1 id=\"not-found-title\">这个文件<br>不在这里</h1><p class=\"explanation\">它可能被移动、重命名，或者这个地址原本就不存在。</p><div class=\"requested\"><span>请求路径</span><code>{path}</code></div><nav aria-label=\"后续操作\"><a class=\"primary\" href=\"/\">浏览根目录 <span aria-hidden=\"true\">→</span></a><a class=\"secondary\" href=\".\">返回上一级</a></nav></section><div class=\"visual\" aria-hidden=\"true\"><div class=\"file-card\"><div class=\"fold\"></div><span class=\"file-label\">FILE</span><strong>404</strong><div class=\"rule long\"></div><div class=\"rule short\"></div><div class=\"tear\"><i></i><i></i><i></i><i></i><i></i></div></div><p>RESOURCE / MISSING</p></div></main>\n</body>\n</html>"
    )
}

fn render_breadcrumbs(relative: &Path) -> String {
    let mut breadcrumbs = String::from("<a href=\"/\">root</a>");
    let mut breadcrumb_path = PathBuf::new();
    for component in relative.components() {
        if let Component::Normal(name) = component {
            breadcrumb_path.push(name);
            let label = escape_html(&name.to_string_lossy());
            let href = url_for_path(&breadcrumb_path, true);
            breadcrumbs.push_str(&format!(
                "<span aria-hidden=\"true\">/</span><a href=\"{href}\">{label}</a>"
            ));
        }
    }
    breadcrumbs
}

fn render_directory_page(root: &Path, directory: &Path, state: &StateStore) -> io::Result<String> {
    let relative = directory.strip_prefix(root).unwrap_or(Path::new(""));
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if review::is_review_sidecar(&entry.path())
            || gallery_comments::is_comments_file(&entry.path())
        {
            continue;
        }
        let file_type = entry.file_type()?;
        let metadata = entry.metadata().ok();
        let is_image = metadata.as_ref().is_some_and(|metadata| metadata.is_file())
            && is_image_file(&entry.path());
        let canonical = is_image
            .then(|| fs::canonicalize(entry.path()))
            .transpose()?;
        let favourite = match canonical.as_deref() {
            Some(path) => state.is_favourite(path)?,
            None => false,
        };
        let deletion_marked = match canonical.as_deref() {
            Some(path) => state.is_deletion_marked(path)?,
            None => false,
        };
        let image_version = canonical
            .as_deref()
            .zip(metadata.as_ref())
            .map(|(_, metadata)| file_version(metadata));
        entries.push(DirectoryEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path(),
            is_dir: metadata
                .as_ref()
                .map_or_else(|| file_type.is_dir(), fs::Metadata::is_dir),
            size: metadata.as_ref().map_or(0, fs::Metadata::len),
            image_version,
            favourite,
            deletion_marked,
        });
    }
    entries.sort_by(|left, right| {
        right
            .favourite
            .cmp(&left.favourite)
            .then_with(|| right.is_dir.cmp(&left.is_dir))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });

    let directory_count = entries.iter().filter(|entry| entry.is_dir).count();
    let file_count = entries.len() - directory_count;
    let image_count = entries
        .iter()
        .filter(|entry| !entry.is_dir && is_image_file(&entry.path))
        .count();
    let gallery_available = should_use_gallery(image_count, file_count);
    let title = relative
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("根目录");

    let breadcrumbs = render_breadcrumbs(relative);

    let mut rows = String::new();
    for entry in entries {
        let entry_relative = entry.path.strip_prefix(root).unwrap_or(&entry.path);
        let href = url_for_path(entry_relative, entry.is_dir);
        let name = escape_html(&entry.name);
        let (kind, detail) = if entry.is_dir {
            ("dir".to_string(), "目录".to_string())
        } else {
            (file_kind(&entry.path).to_string(), human_size(entry.size))
        };
        let is_image = !entry.is_dir && is_image_file(&entry.path);
        let thumb = (!entry.is_dir)
            .then(|| {
                thumbnail_url(
                    &href,
                    &entry.path,
                    entry.size,
                    false,
                    entry.image_version.as_deref(),
                )
            })
            .flatten();
        let gallery_thumb = gallery_available
            .then(|| {
                thumbnail_url(
                    &href,
                    &entry.path,
                    entry.size,
                    true,
                    entry.image_version.as_deref(),
                )
            })
            .flatten();
        let class = match (entry.is_dir, is_image, has_extension(&entry.path, "svg")) {
            (true, _, _) => "folder",
            (false, true, true) => "file image vector",
            (false, true, false) => "file image",
            (false, false, _) => "file",
        };
        let glyph = match (&thumb, &gallery_thumb) {
            (Some(src), Some(gallery_src)) => format!(
                "<span class=\"glyph\" aria-hidden=\"true\"><img data-list-src=\"{src}\" data-gallery-src=\"{gallery_src}\" alt=\"\" decoding=\"async\" draggable=\"false\" onerror=\"this.hidden=true;this.closest('.glyph').classList.add('thumbnail-error')\"></span>"
            ),
            (Some(src), None) => format!(
                "<span class=\"glyph\" aria-hidden=\"true\"><img data-list-src=\"{src}\" alt=\"\" decoding=\"async\" draggable=\"false\" onerror=\"this.hidden=true;this.closest('.glyph').classList.add('thumbnail-error')\"></span>"
            ),
            (None, _) => "<span class=\"glyph\" aria-hidden=\"true\"></span>".to_string(),
        };
        let glyph = if is_image {
            let favourite_hidden = if entry.favourite { "" } else { " hidden" };
            let deletion_hidden = if entry.deletion_marked { "" } else { " hidden" };
            glyph.replacen(
                "</span>",
                &format!("<span class=\"favourite-mark\" title=\"已点赞\"{favourite_hidden}><svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"M12 21c-.45 0-.85-.18-1.2-.5l-6.7-6.2C1.35 11.75 1.2 7.75 3.7 5.2 6.05 2.8 10 2.85 12 5.45c2-2.6 5.95-2.65 8.3-.25 2.5 2.55 2.35 6.55-.4 9.1l-6.7 6.2c-.35.32-.75.5-1.2.5Z\"></path></svg></span><span class=\"deletion-mark\" title=\"待删除\"{deletion_hidden}>待删除</span></span>"),
                1,
            )
        } else {
            glyph
        };
        let preview = if is_image {
            let version = entry.image_version.as_deref().unwrap_or_default();
            let original_src = format!("{href}?mode=asset&amp;v={version}");
            let preview_src = gallery_preview_url(
                &href,
                &entry.path,
                entry.size,
                entry.image_version.as_deref(),
            )
            .unwrap_or_else(|| original_src.clone());
            format!(
                " data-favourite=\"{}\" data-deletion-marked=\"{}\" data-file-path=\"{}\" data-file-size=\"{}\" data-preview-src=\"{preview_src}\" data-original-src=\"{original_src}\" data-list-href=\"{href}\" data-gallery-href=\"{href}?view=gallery\"",
                entry.favourite,
                entry.deletion_marked,
                escape_html(&entry_relative.to_string_lossy()),
                escape_html(&detail)
            )
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<a class=\"entry {class}\" href=\"{href}\"{preview}>{glyph}<span class=\"entry-name\">{name}</span><span class=\"kind\">{kind}</span><span class=\"detail\">{detail}</span><span class=\"arrow\" aria-hidden=\"true\">→</span></a>"
        ));
    }
    let empty_hidden = if rows.is_empty() { "" } else { " hidden" };
    rows.push_str(&format!(
        "<div class=\"empty\" id=\"directory-empty\" role=\"status\"{empty_hidden}><span aria-hidden=\"true\">∅</span><p>这个目录是空的</p></div>"
    ));
    rows.push_str("<h2 class=\"gallery-other-heading\">其他文件</h2>");

    let title = escape_html(title);
    let gallery_toggle = if gallery_available {
        "<button class=\"view-toggle\" id=\"gallery-toggle\" type=\"button\" aria-pressed=\"false\"><span aria-hidden=\"true\">▦</span> Gallery</button>"
    } else {
        ""
    };
    let gallery_tools = if gallery_available {
        GALLERY_ORGANISE_TOOLS
    } else {
        ""
    };
    Ok(format!(
        "<!doctype html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\n<title>{title} · 文件浏览</title>\n<style>{DIRECTORY_CSS}</style>\n</head>\n<body>\n<main><nav class=\"breadcrumbs\" aria-label=\"当前位置\">{breadcrumbs}</nav><header><p class=\"eyebrow\">HTTP / DIRECTORY</p><h1>{title}</h1><p class=\"summary\">{directory_count} 个目录 · {file_count} 个文件</p>{gallery_toggle}<input class=\"directory-search\" id=\"directory-search\" type=\"search\" aria-label=\"搜索文件名\" placeholder=\"搜索当前目录的文件名…\" autocomplete=\"off\">{gallery_tools}<p class=\"directory-notice\" id=\"directory-notice\" role=\"status\" hidden></p></header><section class=\"listing\" aria-label=\"目录内容\">{rows}</section></main><nav class=\"scroll-jumps\" id=\"scroll-jumps\" aria-label=\"页面快速跳转\" hidden><button id=\"scroll-to-top\" type=\"button\" aria-label=\"回到顶部\" title=\"回到顶部\"><svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"m6 14 6-6 6 6\"></path><path d=\"M6 19h12\"></path></svg></button><button id=\"scroll-to-bottom\" type=\"button\" aria-label=\"回到底部\" title=\"回到底部\"><svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"m6 10 6 6 6-6\"></path><path d=\"M6 5h12\"></path></svg></button></nav><div class=\"image-lightbox\" id=\"image-lightbox\" role=\"dialog\" aria-modal=\"true\" aria-label=\"图片预览\" hidden><button class=\"lightbox-close\" type=\"button\" aria-label=\"关闭图片预览\">×</button><div class=\"lightbox-shell\"><div class=\"lightbox-position\" id=\"lightbox-position\" aria-live=\"polite\"></div><nav class=\"lightbox-filmstrip\" id=\"lightbox-filmstrip\" aria-label=\"图片缩略图导航\"></nav><figure><div class=\"lightbox-stage\"><img class=\"lightbox-image\" alt=\"\"><div class=\"favourite-burst\" id=\"favourite-burst\" aria-hidden=\"true\" hidden><svg viewBox=\"0 0 24 24\"><path d=\"M20.8 4.6a5.5 5.5 0 0 0-7.8 0L12 5.7l-1.1-1.1a5.5 5.5 0 0 0-7.8 7.8l1.1 1.1L12 21l7.7-7.5 1.1-1.1a5.5 5.5 0 0 0 0-7.8Z\"></path></svg></div></div><figcaption><span class=\"lightbox-name\"></span><span class=\"lightbox-controls\"><button class=\"preview-step\" id=\"preview-previous\" type=\"button\" aria-label=\"上一张\" title=\"上一张\">←</button><button class=\"favourite-toggle\" id=\"favourite-toggle\" type=\"button\" aria-label=\"点赞 (f)\" aria-pressed=\"false\" title=\"点赞 (f)\">♡</button><button class=\"preview-step\" id=\"preview-next\" type=\"button\" aria-label=\"下一张\" title=\"下一张\">→</button><button class=\"deletion-toggle\" id=\"deletion-toggle\" type=\"button\" aria-label=\"标记待删除 (m)\" aria-pressed=\"false\" title=\"标记待删除 (m)\">标记删除</button><button class=\"carousel-toggle\" id=\"carousel-toggle\" type=\"button\" aria-label=\"进入轮播 (p)\" aria-pressed=\"false\" title=\"进入轮播 (p)\">轮播</button></span></figcaption><p class=\"lightbox-error\" id=\"favourite-error\" role=\"status\" hidden></p><p class=\"lightbox-error\" id=\"deletion-mark-error\" role=\"status\" hidden></p></figure></div>{GALLERY_DELETE_DIALOG}</div>{GALLERY_MARKED_DELETE_DIALOG}\n<script>{FILE_SHORTCUT_JS}</script><script>{DIRECTORY_JS}</script>\n</body>\n</html>"
    ))
}

fn url_for_path(path: &Path, is_dir: bool) -> String {
    let mut url = String::from("/");
    let mut first = true;
    for component in path.components() {
        if let Component::Normal(value) = component {
            if !first {
                url.push('/');
            }
            first = false;
            url.push_str(&percent_encode_component(&value.to_string_lossy()));
        }
    }
    if is_dir && !url.ends_with('/') {
        url.push('/');
    }
    url
}

fn percent_encode_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn is_image_file(path: &Path) -> bool {
    file_kind(path) == "IMAGE"
}

fn is_raster_image(path: &Path) -> bool {
    is_image_file(path) && !has_extension(path, "svg")
}

fn should_use_gallery(image_count: usize, file_count: usize) -> bool {
    file_count > 0 && image_count > 0
}

fn thumbnail_url(
    href: &str,
    path: &Path,
    size: u64,
    gallery_mode: bool,
    version: Option<&str>,
) -> Option<String> {
    if size == 0 || size > MAX_THUMBNAIL_SOURCE || !is_image_file(path) {
        return None;
    }
    let version = version?;
    if has_extension(path, "svg") {
        Some(format!("{href}?mode=asset&amp;v={version}"))
    } else if gallery_mode {
        Some(format!("{href}?mode=gallery-thumb&amp;v={version}"))
    } else {
        Some(format!("{href}?mode=thumb&amp;v={version}"))
    }
}

fn gallery_preview_url(
    href: &str,
    path: &Path,
    size: u64,
    version: Option<&str>,
) -> Option<String> {
    if size == 0
        || size > MAX_THUMBNAIL_SOURCE
        || !is_raster_image(path)
        || has_extension(path, "gif")
    {
        return None;
    }
    Some(format!("{href}?mode=gallery-preview&amp;v={}", version?))
}

fn file_version(metadata: &fs::Metadata) -> String {
    let mut digest = Sha1::new();
    digest.update(metadata.len().to_le_bytes());
    if let Ok(modified) = metadata.modified() {
        match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => {
                digest.update(duration.as_secs().to_le_bytes());
                digest.update(duration.subsec_nanos().to_le_bytes());
            }
            Err(error) => {
                let duration = error.duration();
                digest.update(duration.as_secs().to_le_bytes());
                digest.update(duration.subsec_nanos().to_le_bytes());
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        digest.update(metadata.dev().to_le_bytes());
        digest.update(metadata.ino().to_le_bytes());
        digest.update(metadata.ctime().to_le_bytes());
        digest.update(metadata.ctime_nsec().to_le_bytes());
    }
    hex_digest(&digest.finalize())
}

fn send_image_thumbnail(
    stream: &mut TcpStream,
    path: &Path,
    metadata: &fs::Metadata,
    max_edge: u32,
    head_only: bool,
    cache: Option<&ImageCache>,
    requested_version: Option<&str>,
    if_none_match: &str,
) -> io::Result<()> {
    if !is_raster_image(path) {
        return send_text(
            stream,
            415,
            "Unsupported Media Type",
            "该文件不支持缩略图\n",
            head_only,
        );
    }
    if metadata.len() == 0 || metadata.len() > MAX_THUMBNAIL_SOURCE {
        return send_text(
            stream,
            413,
            "Content Too Large",
            "图片超过 64 MB，无法生成预览图\n",
            head_only,
        );
    }
    let version = file_version(metadata);
    let rendered = thumbnails::thumbnail(path, &version, max_edge, cache, || {
        let source = fs::read(path)?;
        render_image_thumbnail(&source, max_edge)
    });
    match rendered {
        Ok(thumbnail) => {
            let (bytes, content_type) = thumbnail.as_ref();
            let etag = cache.map(|_| format!("\"{version}-{max_edge}\""));
            let immutable = cache.is_some() && requested_version == Some(version.as_str());
            let modified = send_image_headers(
                stream,
                bytes.len() as u64,
                content_type,
                etag.as_deref(),
                if_none_match,
                immutable,
            )?;
            if modified && !head_only {
                stream.write_all(bytes)?;
            }
            Ok(())
        }
        Err(_) => send_text(
            stream,
            415,
            "Unsupported Media Type",
            "无法生成缩略图\n",
            head_only,
        ),
    }
}

fn render_image_thumbnail(source: &[u8], max_edge: u32) -> io::Result<(Vec<u8>, &'static str)> {
    let mut reader = image::ImageReader::new(Cursor::new(source)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().map_err(image_io_error)?;
    let max_edge = max_edge.min(image.width().max(image.height()));
    let thumb = image.thumbnail(max_edge, max_edge);
    let mut bytes = Vec::new();
    let mut cursor = Cursor::new(&mut bytes);
    if thumb.color().has_alpha() {
        thumb
            .write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(image_io_error)?;
        Ok((bytes, "image/png"))
    } else {
        let rgb = thumb.to_rgb8();
        let quality = if max_edge > GALLERY_THUMBNAIL_MAX_EDGE {
            86
        } else {
            82
        };
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality)
            .encode_image(&rgb)
            .map_err(image_io_error)?;
        Ok((bytes, "image/jpeg"))
    }
}

fn image_io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn file_kind(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(name.as_str(), "dockerfile" | "dockfile" | "containerfile") {
        return "DOCKERFILE";
    }
    if matches!(name.as_str(), "makefile" | "gnumakefile") {
        return "MAKEFILE";
    }
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "HTML",
        "md" => "MD",
        "css" => "CSS",
        "js" | "mjs" => "JS",
        "json" => "JSON",
        "toml" => "TOML",
        "xml" => "XML",
        "yaml" | "yml" => "YAML",
        "conf" | "config" | "cfg" | "ini" => "CONFIG",
        "sh" | "bash" | "zsh" | "fish" => "SHELL",
        "rs" => "RUST",
        "sql" => "SQL",
        "go" => "GO",
        "ts" | "tsx" => "TYPESCRIPT",
        "py" | "pyw" => "PYTHON",
        "java" => "JAVA",
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hxx" => "C/C++",
        "lisp" | "lsp" | "cl" | "el" | "scm" | "ss" | "rkt" => "LISP",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => "IMAGE",
        "drawio" => "DRAWIO",
        "pdf" => "PDF",
        "txt" => "TEXT",
        _ => "FILE",
    }
}

struct TextFile {
    title: String,
    kind: &'static str,
    content: String,
}

fn read_text_file(path: &Path, size: u64) -> io::Result<Option<TextFile>> {
    if size > MAX_TEXT_VIEWER_FILE || has_binary_extension(path) {
        return Ok(None);
    }

    let bytes = fs::read(path)?;
    if has_binary_magic(&bytes) || !looks_like_utf8_text(&bytes) {
        return Ok(None);
    }
    let content = String::from_utf8(bytes)
        .expect("text detection already validated UTF-8")
        .trim_start_matches('\u{feff}')
        .to_string();
    let title = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Text".to_string());
    Ok(Some(TextFile {
        kind: text_kind(path),
        title,
        content,
    }))
}

fn looks_like_utf8_text(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    !text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

fn has_binary_magic(bytes: &[u8]) -> bool {
    bytes.contains(&0)
        || bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"%PDF-")
        || bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"\x1f\x8b")
        || bytes.starts_with(b"\0asm")
        || bytes.starts_with(b"SQLite format 3\0")
        || bytes.starts_with(b"\x00\x01\x00\x00")
        || bytes.starts_with(b"wOFF")
        || bytes.starts_with(b"wOF2")
}

fn has_binary_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "ico"
            | "bmp"
            | "avif"
            | "pdf"
            | "zip"
            | "gz"
            | "bz2"
            | "xz"
            | "7z"
            | "rar"
            | "tar"
            | "wasm"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "a"
            | "o"
            | "class"
            | "jar"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "mp3"
            | "mp4"
            | "wav"
            | "ogg"
            | "webm"
            | "mov"
            | "avi"
            | "sqlite"
            | "db"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "bin"
            | "dat"
            | "pak"
            | "img"
            | "iso"
            | "dmg"
            | "deb"
            | "rpm"
            | "protobuf"
            | "msgpack"
    )
}

fn text_kind(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match name.as_str() {
        "dockerfile" | "dockfile" | "containerfile" => return "DOCKERFILE",
        "makefile" | "gnumakefile" => return "MAKEFILE",
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => return "SHELL",
        ".gitignore" | ".gitattributes" | ".dockerignore" => return "CONFIG",
        ".env" | ".editorconfig" => return "CONFIG",
        _ => {}
    }
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "toml" => "TOML",
        "xml" => "XML",
        "yaml" | "yml" => "YAML",
        "json" | "jsonl" => "JSON",
        "txt" => "TEXT",
        "conf" | "config" | "cfg" | "ini" | "properties" => "CONFIG",
        "sh" | "bash" | "zsh" | "fish" => "SHELL",
        "lisp" | "lsp" | "cl" | "el" | "scm" | "ss" | "rkt" => "LISP",
        "rs" => "RUST",
        "go" => "GO",
        "py" => "PYTHON",
        "rb" => "RUBY",
        "java" => "JAVA",
        "c" | "h" | "cc" | "cpp" | "hpp" => "C/C++",
        "ts" | "tsx" => "TYPESCRIPT",
        "js" | "jsx" | "mjs" => "JAVASCRIPT",
        "css" | "scss" | "sass" | "less" => "CSS",
        "sql" => "SQL",
        "log" => "LOG",
        _ => "TEXT",
    }
}

struct SyntaxAssets {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn syntax_assets() -> &'static SyntaxAssets {
    static ASSETS: OnceLock<SyntaxAssets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        SyntaxAssets {
            syntaxes: two_face::syntax::extra_newlines(),
            theme: themes.themes["base16-ocean.dark"].clone(),
        }
    })
}

fn is_source_code(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "dockerfile" | "dockfile" | "containerfile" | "makefile" | "gnumakefile"
    ) {
        return true;
    }
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "rs" | "sql"
            | "json"
            | "jsonl"
            | "go"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "lisp"
            | "lsp"
            | "cl"
            | "el"
            | "scm"
            | "ss"
            | "rkt"
            | "py"
            | "pyw"
            | "java"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hpp"
            | "hxx"
            | "cs"
            | "rb"
            | "php"
            | "swift"
            | "kt"
            | "kts"
            | "scala"
            | "lua"
            | "dart"
            | "ex"
            | "exs"
            | "erl"
            | "hrl"
            | "hs"
    )
}

fn source_token(path: &Path) -> &str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    match extension.to_ascii_lowercase().as_str() {
        "jsonl" => "json",
        "cjs" | "mjs" | "jsx" => "js",
        "tsx" => "ts",
        "bash" | "zsh" | "fish" => "sh",
        "lsp" | "cl" | "el" | "scm" | "ss" | "rkt" => "lisp",
        "pyw" => "py",
        "cc" | "cxx" | "hpp" | "hxx" => "cpp",
        "kts" => "kt",
        "exs" => "ex",
        "hrl" => "erl",
        _ => extension,
    }
}

fn highlighted_source_lines(content: &str, path: &Path) -> Option<String> {
    if !is_source_code(path) {
        return None;
    }
    let assets = syntax_assets();
    let first_line = content.lines().next().unwrap_or("");
    let syntax = assets
        .syntaxes
        .find_syntax_for_file(path)
        .ok()
        .flatten()
        .or_else(|| assets.syntaxes.find_syntax_by_token(source_token(path)))
        .or_else(|| assets.syntaxes.find_syntax_by_first_line(first_line))?;
    if syntax.name == "Plain Text" {
        return None;
    }

    let mut highlighter = HighlightLines::new(syntax, &assets.theme);
    let mut output = String::new();
    for line in LinesWithEndings::from(content) {
        let regions = highlighter.highlight_line(line, &assets.syntaxes).ok()?;
        let highlighted = styled_line_to_highlighted_html(&regions, IncludeBackground::No).ok()?;
        output.push_str("<span class=\"line\">");
        output.push_str(highlighted.trim_end_matches(['\r', '\n']));
        output.push_str("</span>");
    }
    if output.is_empty() || content.ends_with('\n') {
        output.push_str("<span class=\"line\"></span>");
    }
    Some(output)
}

fn render_text_page(content: &str, title: &str, kind: &str, path: &Path) -> String {
    let highlighted = highlighted_source_lines(content, path);
    let is_highlighted = highlighted.is_some();
    let lines = highlighted.unwrap_or_else(|| {
        let mut output = String::new();
        for line in content.split('\n') {
            output.push_str("<span class=\"line\">");
            output.push_str(&escape_html(line.trim_end_matches('\r')));
            output.push_str("</span>");
        }
        output
    });
    let body_class = if is_highlighted {
        " class=\"highlighted\""
    } else {
        ""
    };
    let line_count = content.split('\n').count();
    let title = escape_html(title);
    format!(
        "<!doctype html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><title>{title}</title><style>{TEXT_CSS}</style></head><body{body_class}><header><button class=\"back\" id=\"history-back\" type=\"button\" aria-label=\"返回上一页\">←</button><span class=\"kind\">{kind}</span><span class=\"filename\">{title}</span><span class=\"meta\">{line_count} 行 · {size}</span><button id=\"wrap\" type=\"button\">自动换行</button><button id=\"copy\" type=\"button\">复制</button><a class=\"raw\" href=\"?mode=raw\">Raw</a></header><main><pre id=\"code\"><code>{lines}</code></pre></main><div id=\"toast\" role=\"status\"></div><script>{TEXT_JS}</script></body></html>",
        size = human_size(content.len() as u64)
    )
}

fn render_svg_page(title: &str, size: u64) -> String {
    let title = escape_html(title);
    format!(
        "<!doctype html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><title>{title}</title><style>{SVG_CSS}</style></head><body><header><button class=\"back\" id=\"history-back\" type=\"button\" aria-label=\"返回上一页\">←</button><span class=\"kind\">SVG</span><span class=\"filename\">{title}</span><span class=\"meta\">{size}</span><button id=\"scale\" type=\"button\">原始尺寸</button><a class=\"raw\" href=\"?mode=raw\">Raw</a></header><main class=\"canvas\"><img id=\"artwork\" src=\"?mode=asset\" alt=\"{title}\"><p id=\"error\" hidden>无法渲染这个 SVG 文件</p></main><script>{SVG_JS}</script></body></html>",
        size = human_size(size)
    )
}

fn render_image_page(title: &str, kind: &str, size: u64) -> String {
    let title = escape_html(title);
    let kind = escape_html(kind);
    format!(
        "<!doctype html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><title>{title}</title><style>{SVG_CSS}</style></head><body><header><button class=\"back\" id=\"history-back\" type=\"button\" aria-label=\"返回上一页\">←</button><span class=\"kind\">{kind}</span><span class=\"filename\">{title}</span><span class=\"meta\">{size}</span><button id=\"scale\" type=\"button\">原始尺寸</button><a class=\"raw\" href=\"?mode=asset\">原图</a></header><main class=\"canvas\"><img id=\"artwork\" src=\"?mode=asset\" alt=\"{title}\"><p id=\"error\" hidden>无法渲染这张图片</p></main><script>{SVG_JS}</script></body></html>",
        size = human_size(size)
    )
}

fn escape_json_string(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0c}' => escaped.push_str("\\f"),
            character if character <= '\u{1f}' => {
                let code = character as u8;
                escaped.push_str("\\u00");
                escaped.push(char::from(HEX[(code >> 4) as usize]));
                escaped.push(char::from(HEX[(code & 0x0f) as usize]));
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn render_drawio_page(diagram: &str, title: &str, size: u64) -> String {
    let config = format!(
        "{{\"highlight\":\"#5b5bd6\",\"nav\":true,\"resize\":true,\"toolbar\":\"zoom layers lightbox\",\"xml\":{}}}",
        escape_json_string(diagram.trim_start_matches('\u{feff}'))
    );
    let config = escape_html(&config);
    let title = escape_html(title);
    format!(
        "<!doctype html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'self' data: blob:; connect-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; object-src 'none'; base-uri 'none'; frame-src 'none'; worker-src 'self' blob:\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><title>{title}</title><style>{DRAWIO_CSS}</style></head><body><header><button class=\"back\" id=\"history-back\" type=\"button\" aria-label=\"返回上一页\">←</button><span class=\"kind\">DRAWIO</span><span class=\"filename\">{title}</span><span class=\"meta\">{size}</span><a class=\"raw\" href=\"?mode=raw\">Raw</a></header><main class=\"canvas\"><div id=\"viewer-status\" class=\"viewer-status\"><span class=\"spinner\" aria-hidden=\"true\"></span><strong>正在渲染图表</strong><small>本地 Draw.io Viewer</small></div><div class=\"mxgraph\" data-mxgraph=\"{config}\"></div></main><script>{DRAWIO_JS}</script><script src=\"{viewer_path}\" onerror=\"drawioViewerFailed()\"></script></body></html>",
        size = human_size(size),
        viewer_path = DRAWIO_VIEWER_PATH
    )
}

fn render_markdown_page(markdown: &str, title: &str) -> String {
    let article = render_markdown_article(markdown);
    let title = escape_html(title);
    let source = escape_html(markdown);
    format!(
        "<!doctype html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\n<title>{title}</title>\n<style>{MARKDOWN_CSS}</style>\n</head>\n<body class=\"review-closed\">\n<header class=\"topbar\"><button class=\"home\" id=\"history-back\" type=\"button\" aria-label=\"返回上一页\">←</button><span class=\"mark\">MD</span><span class=\"filename\">{title}</span><span class=\"top-actions\"><button class=\"identity-button\" id=\"identity-button\" type=\"button\" title=\"切换审阅身份\"></button><button class=\"button ghost review-toggle\" id=\"review-toggle\" type=\"button\" aria-expanded=\"false\">评论 <b id=\"review-count\">0</b></button><a class=\"button ghost raw-button\" href=\"?mode=raw\">Raw</a><button class=\"button\" id=\"edit-button\" type=\"button\">编辑</button></span></header>\n<div id=\"reader\" class=\"reader-layout\"><aside id=\"toc\" aria-label=\"文档目录\"></aside><main class=\"paper\"><article id=\"article\">{article}</article></main><aside id=\"review-panel\" class=\"review-panel\" aria-label=\"审阅评论\"><header class=\"review-header\"><div><span>REVIEW</span><strong>审阅讨论</strong></div><button id=\"review-close\" type=\"button\" aria-label=\"收起评论\">×</button></header><div class=\"review-file-actions\"><a id=\"open-review-file\" target=\"_blank\">打开评论文件</a><button id=\"copy-review-path\" type=\"button\">复制评论文件路径</button></div><div class=\"review-presence\"><i aria-hidden=\"true\"></i><span id=\"review-users\">正在连接…</span></div><button class=\"document-comment\" id=\"document-comment\" type=\"button\">＋ 全文评论</button><div class=\"review-composer\" id=\"review-composer\" hidden><p id=\"composer-scope\"></p><label for=\"comment-body\">写下需要讨论或修改的内容</label><textarea id=\"comment-body\" rows=\"4\"></textarea><div><button class=\"text-button\" id=\"composer-cancel\" type=\"button\">取消</button><button class=\"button\" id=\"composer-submit\" type=\"button\">提交评论</button></div></div><nav class=\"review-filters\" aria-label=\"评论状态\"><button class=\"active\" type=\"button\" data-review-filter=\"open\">待处理 <b>0</b></button><button type=\"button\" data-review-filter=\"addressed\">待确认 <b>0</b></button><button type=\"button\" data-review-filter=\"resolved\">已解决 <b>0</b></button></nav><div class=\"review-complete\" id=\"review-complete\" hidden><strong>审阅已完成</strong><span>可以交给 AI 执行</span></div><div class=\"review-error\" id=\"review-error\" role=\"status\" hidden></div><div class=\"comment-list\" id=\"comment-list\"></div></aside></div>\n<button class=\"selection-comment\" id=\"selection-comment\" type=\"button\" hidden>＋ 添加批注</button><div class=\"toast\" id=\"review-toast\" role=\"status\" hidden></div><dialog class=\"identity-dialog\" id=\"identity-dialog\"><form method=\"dialog\"><span class=\"dialog-kicker\">REVIEW IDENTITY</span><h2>你以什么身份参与审阅？</h2><p>输入一个方便其他审阅者辨认的名称，浏览器会在此设备上记住它。</p><label for=\"identity-input\">审阅人名称</label><input id=\"identity-input\" name=\"identity\" autocomplete=\"username\" placeholder=\"例如：小明、Alice、dev-01\" required><span class=\"identity-error\" id=\"identity-error\"></span><button class=\"button\" id=\"identity-submit\" value=\"confirm\">进入审阅</button></form></dialog>\n<section id=\"editor\" class=\"editor-shell\" hidden><div class=\"editor-toolbar\"><div class=\"format-tools\" role=\"toolbar\" aria-label=\"Markdown 格式\"><button type=\"button\" data-format=\"heading\" title=\"标题\">H</button><button type=\"button\" data-format=\"bold\" title=\"粗体\"><strong>B</strong></button><button type=\"button\" data-format=\"italic\" title=\"斜体\"><em>I</em></button><button type=\"button\" data-format=\"link\" title=\"链接\">↗</button><button type=\"button\" data-format=\"quote\" title=\"引用\">❯</button><button type=\"button\" data-format=\"code\" title=\"代码\">&lt;/&gt;</button><button type=\"button\" data-format=\"list\" title=\"列表\">≡</button><button type=\"button\" data-format=\"task\" title=\"任务\">☑</button></div><span class=\"collaboration-status\" role=\"status\"><i id=\"connection-dot\" aria-hidden=\"true\"></i><span id=\"save-status\">未连接</span><span id=\"presence\"></span></span><button class=\"button ghost\" id=\"cancel-button\" type=\"button\">退出编辑</button><button class=\"button\" id=\"save-button\" type=\"button\">立即保存</button></div><div class=\"editor-panes\"><div class=\"pane preview-pane\"><span>PREVIEW</span><iframe id=\"preview\" title=\"Markdown 实时预览\"></iframe></div><label class=\"pane source-pane\"><span>MARKDOWN</span><textarea id=\"source\" spellcheck=\"false\" disabled>{source}</textarea></label></div></section>\n<script src=\"{MERMAID_PATH}\"></script><script>{MARKDOWN_MERMAID_JS}</script>\n<script src=\"{YJS_PATH}\"></script><script>{MARKDOWN_JS}</script>\n</body>\n</html>"
    )
}

fn markdown_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_GFM);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    options.insert(Options::ENABLE_DEFINITION_LIST);
    options
}

fn render_markdown_article(markdown: &str) -> String {
    // Embedded HTML is shown as text so untrusted Markdown cannot inject scripts.
    let mut source_byte = 0;
    let mut source_utf16 = 0;
    let parser = Parser::new_ext(markdown, markdown_options())
        .into_offset_iter()
        .flat_map(|(event, range)| {
            let is_anchor = matches!(
                &event,
                Event::Start(
                    Tag::Paragraph
                        | Tag::Heading { .. }
                        | Tag::BlockQuote(_)
                        | Tag::CodeBlock(_)
                        | Tag::List(_)
                        | Tag::FootnoteDefinition(_)
                        | Tag::DefinitionList
                        | Tag::Table(_)
                ) | Event::Rule
                    | Event::Html(_)
            );
            let event = match event {
                Event::Html(value) | Event::InlineHtml(value) => Event::Text(value),
                event => event,
            };
            let event_start_utf16 = if range.start >= source_byte {
                let offset = source_utf16
                    + markdown[source_byte..range.start].encode_utf16().count();
                source_byte = range.start;
                source_utf16 = offset;
                offset
            } else {
                markdown[..range.start].encode_utf16().count()
            };
            let event_end_utf16 = event_start_utf16
                + markdown[range.start..range.end].encode_utf16().count();
            let is_source_run = matches!(&event, Event::Text(_) | Event::Code(_));
            let mut events = Vec::with_capacity(4);
            if is_anchor {
                events.push(Event::Html(CowStr::Boxed(
                    format!(
                        "<span class=\"sync-anchor\" data-source-offset=\"{event_start_utf16}\" aria-hidden=\"true\"></span>"
                    )
                    .into_boxed_str(),
                )));
            }
            if is_source_run {
                events.push(Event::Html(CowStr::Boxed(
                    format!(
                        "<span class=\"source-run\" data-source-start=\"{event_start_utf16}\" data-source-end=\"{event_end_utf16}\">"
                    )
                    .into_boxed_str(),
                )));
            }
            events.push(event);
            if is_source_run {
                events.push(Event::Html(CowStr::Borrowed("</span>")));
            }
            events
        });
    let mut article = String::new();
    html::push_html(&mut article, parser);
    article
}

fn render_markdown_preview_page(markdown: &str) -> String {
    let article = render_markdown_article(markdown);
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><style>{MARKDOWN_CSS}</style></head><body class=\"preview-body\"><main class=\"paper preview-paper\"><article>{article}</article></main><script src=\"{MERMAID_PATH}\"></script><script>{MARKDOWN_MERMAID_JS}</script></body></html>"
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

const NOT_FOUND_CSS: &str = r#"
:root { color-scheme:light dark; --canvas:#f3f5fb; --surface:#fff; --ink:#202334; --muted:#6f7487; --line:#dce0eb; --accent:#5b5bd6; --accent-dark:#4545bb; --accent-soft:#e8e8ff; --shadow:rgba(49,53,87,.14); }
* { box-sizing:border-box; }
html,body { min-height:100%; }
body { display:grid; place-items:center; margin:0; padding:clamp(1.25rem,4vw,3rem); color:var(--ink); background:radial-gradient(circle at 14% 12%,rgba(91,91,214,.11),transparent 26rem),linear-gradient(135deg,transparent 0 49.7%,rgba(91,91,214,.045) 49.8% 50.2%,transparent 50.3%) var(--canvas); background-size:auto,32px 32px; font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC",sans-serif; }
main { display:grid; grid-template-columns:minmax(0,1.08fr) minmax(18rem,.92fr); align-items:center; width:min(100%,980px); min-height:min(650px,calc(100vh - 6rem)); overflow:hidden; border:1px solid var(--line); border-radius:1.5rem; background:var(--surface); box-shadow:0 28px 80px var(--shadow); }
.message { padding:clamp(2rem,7vw,5.5rem); }
.status { display:flex; align-items:center; gap:.6rem; margin:0 0 2rem; color:var(--accent); font:750 .72rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.14em; }
.status span { width:.55rem; height:.55rem; border-radius:50%; background:var(--accent); box-shadow:0 0 0 .35rem var(--accent-soft); }
h1 { max-width:10ch; margin:0; font:720 clamp(2.25rem,4.2vw,3.5rem)/1.08 ui-rounded,"SF Pro Rounded","Nunito Sans",Inter,ui-sans-serif,sans-serif; letter-spacing:-.035em; }
.explanation { max-width:27rem; margin:1.65rem 0 0; color:var(--muted); font-size:clamp(.95rem,1.4vw,1.05rem); line-height:1.75; }
.requested { display:grid; gap:.55rem; margin:2rem 0 2.25rem; }
.requested span { color:var(--muted); font:650 .68rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.1em; }
.requested code { display:block; width:100%; max-width:30rem; padding:.8rem 1rem; border:1px solid var(--line); border-radius:.65rem; color:var(--ink); background:var(--canvas); font:550 .82rem/1.5 ui-monospace,SFMono-Regular,Consolas,monospace; overflow-wrap:anywhere; white-space:pre-wrap; }
nav { display:flex; align-items:center; gap:1rem; flex-wrap:wrap; }
nav a { display:inline-flex; align-items:center; justify-content:center; min-height:2.9rem; border-radius:.7rem; font-size:.86rem; font-weight:700; text-decoration:none; transition:transform .16s ease,background .16s ease,border-color .16s ease; }
.primary { gap:1.2rem; padding:.75rem 1.05rem  .75rem 1.2rem; color:#fff; background:var(--accent); box-shadow:0 8px 22px rgba(91,91,214,.24); }
.primary:hover { background:var(--accent-dark); transform:translateY(-2px); }
.primary span { font-size:1.1rem; }
.secondary { padding:.75rem .35rem; color:var(--muted); }
.secondary:hover { color:var(--accent); }
.visual { align-self:stretch; display:grid; place-items:center; align-content:center; gap:2rem; min-width:0; padding:3rem; overflow:hidden; border-left:1px solid var(--line); background:linear-gradient(145deg,var(--accent-soft),color-mix(in srgb,var(--surface) 65%,var(--accent-soft))); }
.visual>p { margin:0; color:var(--accent); font:700 .66rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.18em; }
.file-card { position:relative; width:min(18rem,70vw); aspect-ratio:4/5; padding:2rem; color:var(--ink); background:var(--surface); filter:drop-shadow(0 22px 24px rgba(54,55,112,.18)); transform:rotate(3deg); }
.file-card::after { position:absolute; inset:.75rem; border:1px solid var(--line); content:""; pointer-events:none; }
.fold { position:absolute; top:0; right:0; width:4rem; height:4rem; background:linear-gradient(45deg,var(--accent-soft) 49%,var(--line) 50% 51%,transparent 52%); }
.file-label { position:relative; display:inline-block; z-index:1; margin-top:1.2rem; padding:.35rem .55rem; color:#fff; background:var(--accent); font:750 .65rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.12em; }
.file-card strong { position:relative; z-index:1; display:block; margin:2.2rem 0 2.5rem; color:var(--accent); font:800 clamp(4.5rem,10vw,7rem)/.8 ui-rounded,"SF Pro Rounded",Inter,sans-serif; letter-spacing:-.08em; }
.rule { position:relative; z-index:1; height:.65rem; margin-top:.8rem; border-radius:1rem; background:var(--line); }
.rule.long { width:75%; }
.rule.short { width:48%; }
.tear { position:absolute; right:-1px; bottom:2.2rem; left:-1px; display:flex; align-items:center; justify-content:space-between; border-top:2px dashed var(--accent); }
.tear i { width:1rem; height:1rem; margin-top:-.5rem; border-radius:50%; background:var(--accent-soft); }
:focus-visible { outline:3px solid color-mix(in srgb,var(--accent) 55%,transparent); outline-offset:3px; }
@media (max-width:760px) { body { display:block; padding:0; background:var(--surface); } main { display:block; min-height:100vh; min-height:100dvh; border:0; border-radius:0; box-shadow:none; } .message { padding:3rem 1.5rem 2.5rem; } .status { margin-bottom:1.5rem; } h1 { font-size:clamp(2.35rem,11vw,3.2rem); } .visual { min-height:23rem; border-top:1px solid var(--line); border-left:0; } .file-card { width:12rem; padding:1.5rem; } .file-card strong { margin:1.5rem 0 2rem; font-size:4.5rem; } }
@media (prefers-color-scheme:dark) { :root { --canvas:#10121a; --surface:#191c27; --ink:#eef0f7; --muted:#9ca2b3; --line:#323746; --accent:#aaa7ff; --accent-dark:#c0bdff; --accent-soft:#292942; --shadow:rgba(0,0,0,.3); } .primary { color:#171826; background:#aaa7ff; box-shadow:0 8px 24px rgba(90,85,190,.22); } .primary:hover { background:#c0bdff; } }
@media (prefers-reduced-motion:reduce) { nav a { transition:none; } .primary:hover { transform:none; } }
"#;

const TEXT_CSS: &str = r#"
:root { --file-header-padding:max(1rem,calc((100vw - 1280px)/2)); color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#272a38; --muted:#73788b; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --gutter:#f0f2f8; }
* { box-sizing:border-box; }
html,body { min-height:100%; }
body { margin:0; color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC",sans-serif; }
header { position:sticky; top:0; z-index:2; display:flex; align-items:center; gap:.65rem; min-height:3.6rem; padding:.7rem var(--file-header-padding); border-bottom:1px solid var(--line); background:color-mix(in srgb,var(--paper) 90%,transparent); backdrop-filter:blur(16px); }
.back { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.5rem; color:var(--muted); text-decoration:none; }
.back:hover { color:var(--accent); background:var(--accent-soft); }
.kind { flex:0 0 auto; padding:.28rem .5rem; border-radius:.35rem; color:white; background:var(--accent); font:750 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.05em; }
.filename { overflow:hidden; font:650 .82rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; }
.meta { margin-left:auto; color:var(--muted); font:500 .7rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; white-space:nowrap; }
button,.raw { min-height:2rem; padding:.45rem .65rem; border:1px solid var(--line); border-radius:.45rem; color:var(--ink); background:var(--surface); font:650 .72rem/1 ui-sans-serif,-apple-system,sans-serif; text-decoration:none; cursor:pointer; }
button:hover,.raw:hover { border-color:var(--accent); color:var(--accent); }
main { width:min(100% - 2rem,1280px); margin:clamp(1rem,4vw,3rem) auto; overflow:hidden; border:1px solid var(--line); border-radius:.85rem; background:var(--surface); box-shadow:0 22px 60px rgba(54,59,92,.08); }
pre { overflow:auto; min-height:calc(100vh - 12.4rem); margin:0; padding:1.1rem 0 2rem; counter-reset:line; tab-size:2; }
code { display:block; min-width:max-content; font:500 .84rem/1.3 ui-monospace,SFMono-Regular,Consolas,"Noto Sans Mono CJK SC",monospace; }
.line { display:block; min-height:1.3em; padding:0 1.25rem 0 0; white-space:pre; counter-increment:line; }
.line::before { position:sticky; left:0; display:inline-block; width:4.2rem; margin-right:1.2rem; border-right:1px solid var(--line); color:var(--muted); background:var(--gutter); content:counter(line); text-align:right; padding-right:1rem; user-select:none; }
.highlighted main { border-color:#252c3d; background:#10151f; box-shadow:0 25px 75px rgba(15,20,31,.24); }
.highlighted pre { background:radial-gradient(circle at 80% -20%,#202d42 0,transparent 34rem),#10151f; }
.highlighted code { font-weight:520; letter-spacing:.005em; }
.highlighted .line { transition:background .1s ease; }
.highlighted .line:hover { background:rgba(143,169,205,.055); }
.highlighted .line::before { border-color:#273044; color:#69758a; background:#0d121b; }
.highlighted .kind { color:#10151f; background:#9bcbb7; }
body.wrap code { min-width:0; }
body.wrap .line { position:relative; padding-left:5.4rem; white-space:pre-wrap; overflow-wrap:anywhere; }
body.wrap .line::before { position:absolute; top:0; bottom:0; left:0; height:auto; margin-right:0; }
#toast { position:fixed; right:1.2rem; bottom:1.2rem; padding:.65rem .85rem; border:1px solid var(--line); border-radius:.55rem; color:var(--ink); background:var(--surface); box-shadow:0 10px 35px rgba(0,0,0,.12); font-size:.78rem; opacity:0; transform:translateY(.5rem); transition:.18s ease; pointer-events:none; }
#toast.show { opacity:1; transform:none; }
:focus-visible { outline:3px solid color-mix(in srgb,var(--accent) 55%,transparent); outline-offset:2px; }
@media (max-width:700px) { :root { --file-header-padding:.65rem; } .meta { display:none; } header button { padding-inline:.5rem; } main { width:100%; margin:0; border-width:0; border-radius:0; box-shadow:none; } pre { min-height:calc(100vh - 6rem); } .line::before { width:3.4rem; margin-right:.8rem; } body.wrap .line { padding-left:4.2rem; } body.wrap .line::before { margin-right:0; } }
@media (prefers-color-scheme:dark) { :root { --paper:#11131b; --surface:#191c27; --ink:#edf0f7; --muted:#969daf; --line:#303545; --accent:#a9a5ff; --accent-soft:#292943; --gutter:#151822; } body { background-image:radial-gradient(circle at 50% -20%,#252943 0,transparent 38rem); } main { box-shadow:0 22px 60px rgba(0,0,0,.24); } }
@media (prefers-reduced-motion:reduce) { #toast { transition:none; } }
"#;

const TEXT_JS: &str = r#"
const toast = document.querySelector('#toast');
function notify(message) {
  toast.textContent = message;
  toast.classList.add('show');
  clearTimeout(notify.timer);
  notify.timer = setTimeout(() => toast.classList.remove('show'), 1300);
}
document.querySelector('#wrap').addEventListener('click', (event) => {
  const wrapped = document.body.classList.toggle('wrap');
  event.currentTarget.textContent = wrapped ? '取消换行' : '自动换行';
});
document.querySelector('#copy').addEventListener('click', async () => {
  try {
    const response = await fetch('?mode=raw');
    if (!response.ok) throw new Error();
    await navigator.clipboard.writeText(await response.text());
    notify('已复制原始文本');
  } catch (_) {
    notify('复制失败');
  }
});
"#;

const SVG_CSS: &str = r#"
:root { --file-header-padding:max(1rem,calc((100vw - 1440px)/2)); color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#272a38; --muted:#73788b; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --grid:#dfe3ec; }
* { box-sizing:border-box; }
html,body { width:100%; min-height:100%; }
body { margin:0; color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC",sans-serif; }
header { position:sticky; top:0; z-index:2; display:flex; align-items:center; gap:.65rem; min-height:3.6rem; padding:.7rem var(--file-header-padding); border-bottom:1px solid var(--line); background:color-mix(in srgb,var(--paper) 90%,transparent); backdrop-filter:blur(16px); }
.back { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.5rem; color:var(--muted); text-decoration:none; }
.back:hover { color:var(--accent); background:var(--accent-soft); }
.kind { flex:0 0 auto; padding:.28rem .5rem; border-radius:.35rem; color:white; background:var(--accent); font:750 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.05em; }
.filename { overflow:hidden; font:650 .82rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; }
.meta { margin-left:auto; color:var(--muted); font:500 .7rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; white-space:nowrap; }
button,.raw { min-height:2rem; padding:.45rem .65rem; border:1px solid var(--line); border-radius:.45rem; color:var(--ink); background:var(--surface); font:650 .72rem/1 ui-sans-serif,-apple-system,sans-serif; text-decoration:none; cursor:pointer; }
button:hover,.raw:hover { border-color:var(--accent); color:var(--accent); }
.canvas { display:grid; place-items:center; width:min(100% - 2rem,1440px); height:calc(100vh - 8rem); height:calc(100dvh - 8rem); min-height:18rem; margin:1rem auto; overflow:auto; border:1px solid var(--line); border-radius:.9rem; background-color:var(--surface); background-image:linear-gradient(45deg,var(--grid) 25%,transparent 25%),linear-gradient(-45deg,var(--grid) 25%,transparent 25%),linear-gradient(45deg,transparent 75%,var(--grid) 75%),linear-gradient(-45deg,transparent 75%,var(--grid) 75%); background-position:0 0,0 8px,8px -8px,-8px 0; background-size:16px 16px; box-shadow:0 22px 60px rgba(54,59,92,.08); }
#artwork { display:block; max-width:calc(100% - 3rem); max-height:calc(100% - 3rem); }
body.actual .canvas { place-items:start; padding:1.5rem; }
body.actual #artwork { max-width:none; max-height:none; }
#error { align-self:center; justify-self:center; padding:1rem 1.25rem; border:1px solid var(--line); border-radius:.65rem; color:var(--muted); background:var(--surface); }
:focus-visible { outline:3px solid color-mix(in srgb,var(--accent) 55%,transparent); outline-offset:2px; }
@media (max-width:700px) { :root { --file-header-padding:.65rem; } .meta { display:none; } .canvas { width:100%; height:calc(100vh - 6rem); height:calc(100dvh - 6rem); margin:0; border-width:0; border-radius:0; } #artwork { max-width:calc(100% - 2rem); max-height:calc(100% - 2rem); } }
@media (prefers-color-scheme:dark) { :root { --paper:#11131b; --surface:#191c27; --ink:#edf0f7; --muted:#969daf; --line:#303545; --accent:#a9a5ff; --accent-soft:#292943; --grid:#292d39; } body { background-image:radial-gradient(circle at 50% -20%,#252943 0,transparent 38rem); } .canvas { box-shadow:0 22px 60px rgba(0,0,0,.24); } }
"#;

const SVG_JS: &str = r#"
const artwork = document.querySelector('#artwork');
const error = document.querySelector('#error');
artwork.addEventListener('error', () => {
  artwork.hidden = true;
  error.hidden = false;
});
document.querySelector('#scale').addEventListener('click', (event) => {
  const actual = document.body.classList.toggle('actual');
  event.currentTarget.textContent = actual ? '适应画布' : '原始尺寸';
});
"#;

const DRAWIO_CSS: &str = r#"
:root { --file-header-padding:max(1rem,calc((100vw - 1500px)/2)); color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#272a38; --muted:#73788b; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --grid:#e7e9f1; }
* { box-sizing:border-box; }
html,body { width:100%; min-height:100%; }
body { margin:0; color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC",sans-serif; }
header { position:sticky; top:0; z-index:3; display:flex; align-items:center; gap:.65rem; min-height:3.6rem; padding:.7rem var(--file-header-padding); border-bottom:1px solid var(--line); background:color-mix(in srgb,var(--paper) 90%,transparent); backdrop-filter:blur(16px); }
.back { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.5rem; color:var(--muted); text-decoration:none; }
.back:hover { color:var(--accent); background:var(--accent-soft); }
.kind { flex:0 0 auto; padding:.28rem .5rem; border-radius:.35rem; color:white; background:var(--accent); font:750 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.05em; }
.filename { overflow:hidden; font:650 .82rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; }
.meta { margin-left:auto; color:var(--muted); font:500 .7rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; white-space:nowrap; }
.raw { min-height:2rem; padding:.55rem .65rem; border:1px solid var(--line); border-radius:.45rem; color:var(--ink); background:var(--surface); font:650 .72rem/1 ui-sans-serif,-apple-system,sans-serif; text-decoration:none; }
.raw:hover { border-color:var(--accent); color:var(--accent); }
.canvas { position:relative; width:min(100% - 2rem,1500px); height:calc(100vh - 8rem); height:calc(100dvh - 8rem); min-height:22rem; margin:1rem auto; overflow:auto; border:1px solid var(--line); border-radius:.9rem; background-color:var(--surface); background-image:linear-gradient(var(--grid) 1px,transparent 1px),linear-gradient(90deg,var(--grid) 1px,transparent 1px); background-size:24px 24px; box-shadow:0 22px 60px rgba(54,59,92,.08); }
.mxgraph { min-width:100%; min-height:100%; padding:2rem; border:1px solid transparent; }
.viewer-status { position:absolute; inset:0; z-index:2; display:grid; place-content:center; justify-items:center; gap:.55rem; color:var(--ink); background:color-mix(in srgb,var(--surface) 88%,transparent); text-align:center; backdrop-filter:blur(4px); }
.viewer-status[hidden] { display:none; }
.viewer-status small { color:var(--muted); font:.65rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.08em; text-transform:uppercase; }
.spinner { width:1.65rem; height:1.65rem; margin-bottom:.25rem; border:2px solid var(--line); border-top-color:var(--accent); border-radius:50%; animation:spin .7s linear infinite; }
.viewer-status.error .spinner { display:none; }
.viewer-status.error strong { color:#bd3e52; }
:focus-visible { outline:3px solid color-mix(in srgb,var(--accent) 55%,transparent); outline-offset:2px; }
@keyframes spin { to { transform:rotate(360deg); } }
@media (max-width:700px) { :root { --file-header-padding:.65rem; } .meta { display:none; } .canvas { width:100%; height:calc(100vh - 6rem); height:calc(100dvh - 6rem); margin:0; border-width:0; border-radius:0; } .mxgraph { padding:1rem; } }
@media (prefers-color-scheme:dark) { :root { --paper:#11131b; --surface:#191c27; --ink:#edf0f7; --muted:#969daf; --line:#303545; --accent:#a9a5ff; --accent-soft:#292943; --grid:#242936; } body { background-image:radial-gradient(circle at 50% -20%,#252943 0,transparent 38rem); } .canvas { box-shadow:0 22px 60px rgba(0,0,0,.24); } }
@media (prefers-reduced-motion:reduce) { .spinner { animation:none; } }
"#;

const DRAWIO_JS: &str = r#"
const viewerStatus = document.querySelector('#viewer-status');
window.RESOURCE_BASE = '/__http_file_server/drawio-assets';
window.STENCIL_PATH = '/__http_file_server/drawio-assets/stencils';
window.SHAPES_PATH = '/__http_file_server/drawio-assets/shapes';
window.IMAGE_PATH = '/__http_file_server/drawio-assets/images';
window.STYLE_PATH = '/__http_file_server/drawio-assets/styles';

function drawioViewerFailed() {
  viewerStatus.classList.add('error');
  viewerStatus.querySelector('strong').textContent = '无法渲染这个 Draw.io 文件';
  viewerStatus.querySelector('small').textContent = '可使用 Raw 查看原始 XML';
}

window.onDrawioViewerLoad = () => {
  try {
    GraphViewer.processElements();
    viewerStatus.hidden = true;
  } catch (_) {
    drawioViewerFailed();
  }
};
"#;

const GALLERY_ORGANISE_TOOLS: &str = r#"
<div class="gallery-organise" id="gallery-organise" role="group" aria-label="整理当前目录的已点赞图片">
  <span class="organise-label">整理到</span>
  <button type="button" data-move-images="favourites" title="将当前目录全部已点赞图片移入 favourites/" aria-label="将全部已点赞图片移入 favourites 目录"><span class="organise-heart" aria-hidden="true">♥</span> favourites <span class="organise-arrow" aria-hidden="true">↗</span></button>
  <button id="deletion-filter" type="button" aria-pressed="false"><span id="deletion-filter-label">查看待删除列表</span> <span id="deletion-count">0</span></button>
  <button id="delete-marked-images" class="delete-marked-images" type="button" hidden>删除所有已标记图片</button>
  <button id="clear-deletion-marks" type="button" hidden>取消所有删除标记</button>
</div>
"#;

const GALLERY_DELETE_DIALOG: &str = r#"
<dialog class="delete-dialog" id="delete-dialog" aria-labelledby="delete-title" aria-describedby="delete-description">
  <h2 id="delete-title">删除这张图片？</h2>
  <p class="delete-name" id="delete-name"></p>
  <p id="delete-description">图片将从磁盘中删除，此操作无法撤销。</p>
  <p class="delete-error" id="delete-error" role="alert" hidden></p>
  <div class="delete-actions"><button id="delete-cancel" type="button" autofocus>取消</button><button id="delete-confirm" type="button">删除图片</button></div>
</dialog>
"#;

const GALLERY_MARKED_DELETE_DIALOG: &str = r#"
<dialog class="delete-dialog" id="delete-marked-dialog" aria-labelledby="delete-marked-title" aria-describedby="delete-marked-description">
  <h2 id="delete-marked-title">删除所有已标记图片？</h2>
  <p id="delete-marked-description">当前文件夹中所有标记为待删除的图片都将从磁盘中删除，此操作无法撤销。</p>
  <p class="delete-error" id="delete-marked-error" role="alert" hidden></p>
  <div class="delete-actions"><button id="delete-marked-cancel" type="button" autofocus>取消</button><button id="delete-marked-confirm" type="button">全部删除</button></div>
</dialog>
"#;

const DIRECTORY_JS: &str = r#"
history.scrollRestoration = 'manual';
window.addEventListener('beforeunload', () => {
  history.replaceState({...history.state, scrollX, scrollY}, '');
});

const directoryNotice = document.querySelector('#directory-notice');
if (history.state?.moveNotice) {
  directoryNotice.textContent = history.state.moveNotice;
  directoryNotice.hidden = false;
  const {moveNotice, ...state} = history.state;
  history.replaceState(state, '');
}

const listing = document.querySelector('.listing');
const directorySearch = document.querySelector('#directory-search');
const directoryEmpty = document.querySelector('#directory-empty');
const deletionFilter = document.querySelector('#deletion-filter');
const filterDirectory = () => {
  const query = directorySearch.value.trim().toLowerCase();
  const markedOnly = deletionFilter?.getAttribute('aria-pressed') === 'true';
  let directoryCount = 0;
  let fileCount = 0;
  listing.querySelectorAll('.entry').forEach(entry => {
    const matchesName = entry.querySelector('.entry-name').textContent.toLowerCase().includes(query);
    entry.hidden = !matchesName || (markedOnly && entry.dataset.deletionMarked !== 'true');
    if (entry.hidden) return;
    if (entry.classList.contains('folder')) directoryCount++;
    else fileCount++;
  });
  document.querySelector('.summary').textContent = `${directoryCount} 个目录 · ${fileCount} 个文件`;
  directoryEmpty.hidden = directoryCount + fileCount > 0;
  const otherHeading = listing.querySelector('.gallery-other-heading');
  if (otherHeading) otherHeading.hidden = !Array.from(listing.querySelectorAll('.entry.file:not(.image)')).some(entry => !entry.hidden);
  directoryEmpty.querySelector('p').textContent = markedOnly
    ? '当前文件夹没有待删除图片'
    : query ? '没有匹配的文件或目录' : '这个目录是空的';
};
directorySearch.addEventListener('input', filterDirectory);
filterDirectory();

const scrollJumps = document.querySelector('#scroll-jumps');
const scrollToTop = scrollJumps.querySelector('#scroll-to-top');
const scrollToBottom = scrollJumps.querySelector('#scroll-to-bottom');
let scrollJumpFrame = null;
const updateScrollJumps = () => {
  scrollJumpFrame = null;
  const maximum = Math.max(0, document.documentElement.scrollHeight - innerHeight);
  scrollJumps.hidden = maximum < 48;
  scrollToTop.disabled = scrollY <= 4;
  scrollToBottom.disabled = scrollY >= maximum - 4;
};
const scheduleScrollJumpUpdate = () => {
  if (scrollJumpFrame !== null) return;
  scrollJumpFrame = requestAnimationFrame(updateScrollJumps);
};
const jumpScroll = top => window.scrollTo({
  top,
  behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth'
});
scrollToTop.addEventListener('click', () => jumpScroll(0));
scrollToBottom.addEventListener('click', () => jumpScroll(document.documentElement.scrollHeight));
addEventListener('scroll', scheduleScrollJumpUpdate, {passive: true});
addEventListener('resize', scheduleScrollJumpUpdate);
new ResizeObserver(scheduleScrollJumpUpdate).observe(document.body);
updateScrollJumps();

const galleryToggle = document.querySelector('#gallery-toggle');
if (galleryToggle) {
  const organise = document.querySelector('#gallery-organise');
  const moveFavouriteButton = organise.querySelector('[data-move-images="favourites"]');
  const thumbnails = listing.querySelectorAll('img[data-list-src]');
  const lightbox = document.querySelector('#image-lightbox');
  const lightboxImage = lightbox.querySelector('.lightbox-image');
  const lightboxStage = lightbox.querySelector('.lightbox-stage');
  const lightboxBackdrop = document.createElement('div');
  lightboxBackdrop.className = 'carousel-backdrop';
  lightboxBackdrop.setAttribute('aria-hidden', 'true');
  lightboxStage.prepend(lightboxBackdrop);
  const lightboxPlaceholder = document.createElement('img');
  lightboxPlaceholder.className = 'lightbox-placeholder';
  lightboxPlaceholder.alt = '';
  lightboxPlaceholder.setAttribute('aria-hidden', 'true');
  lightboxPlaceholder.draggable = false;
  lightboxStage.insertBefore(lightboxPlaceholder, lightboxStage.querySelector('.lightbox-image'));
  const imageLoading = document.createElement('div');
  imageLoading.className = 'image-loading';
  imageLoading.hidden = true;
  imageLoading.innerHTML = '<i aria-hidden="true"></i><span>正在载入清晰图片…</span>';
  lightboxStage.append(imageLoading);
  const imageLoadError = document.createElement('div');
  imageLoadError.className = 'image-load-error';
  imageLoadError.hidden = true;
  imageLoadError.innerHTML = '<strong>图片加载失败</strong><button type="button">重新加载</button>';
  lightboxStage.append(imageLoadError);
  const heartParticles = document.createElement('div');
  heartParticles.className = 'heart-particles';
  heartParticles.setAttribute('aria-hidden', 'true');
  lightbox.append(heartParticles);
  for (let index = 0; index < 12; index++) {
    const particle = document.createElement('span');
    particle.hidden = true;
    particle.innerHTML = '<svg viewBox="0 0 24 24"><path d="M20.8 4.6a5.5 5.5 0 0 0-7.8 0L12 5.7l-1.1-1.1a5.5 5.5 0 0 0-7.8 7.8l1.1 1.1L12 21l7.7-7.5 1.1-1.1a5.5 5.5 0 0 0 0-7.8Z"></path></svg>';
    heartParticles.append(particle);
  }
  const lightboxPosition = lightbox.querySelector('#lightbox-position');
  const lightboxFilmstrip = lightbox.querySelector('#lightbox-filmstrip');
  const lightboxCaption = lightbox.querySelector('.lightbox-name');
  const favouriteToggle = lightbox.querySelector('#favourite-toggle');
  const favouriteBurst = lightbox.querySelector('#favourite-burst');
  const favouriteError = lightbox.querySelector('#favourite-error');
  const deletionToggle = lightbox.querySelector('#deletion-toggle');
  const deletionMarkError = lightbox.querySelector('#deletion-mark-error');
  const previewPrevious = lightbox.querySelector('#preview-previous');
  const previewNext = lightbox.querySelector('#preview-next');
  const carouselToggle = lightbox.querySelector('#carousel-toggle');
  const lightboxClose = lightbox.querySelector('.lightbox-close');
  const carouselHud = document.createElement('div');
  carouselHud.className = 'carousel-hud';
  carouselHud.innerHTML = '<span class="carousel-notice" hidden></span><button type="button" data-carousel-pause>暂停</button><button type="button" data-carousel-exit>退出轮播</button>';
  lightbox.append(carouselHud);
  const viewerControls = lightbox.querySelector('.lightbox-controls');
  const svgIcon = paths => `<svg viewBox="0 0 24 24" aria-hidden="true">${paths}</svg>`;
  const strokePath = path => `<path d="${path}"></path>`;
  const commentToggle = document.createElement('button');
  commentToggle.type = 'button';
  commentToggle.className = 'comment-toggle';
  commentToggle.title = '图片评论 (c)';
  commentToggle.setAttribute('aria-label', commentToggle.title);
  commentToggle.setAttribute('aria-expanded', 'false');
  commentToggle.innerHTML = `${svgIcon(strokePath('M6.5 5.5h11a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2H11l-4.5 3v-3a2 2 0 0 1-2-2v-7a2 2 0 0 1 2-2Z') + strokePath('M8 9.5h8M8 12.5h5'))}<b hidden>0</b>`;
  viewerControls.insertBefore(commentToggle, deletionToggle);
  const moreToggle = document.createElement('button');
  moreToggle.type = 'button';
  moreToggle.className = 'viewer-more-toggle';
  moreToggle.title = '更多工具';
  moreToggle.setAttribute('aria-label', moreToggle.title);
  moreToggle.setAttribute('aria-expanded', 'false');
  moreToggle.innerHTML = svgIcon(strokePath('m7 9.5 5 5 5-5'));
  viewerControls.append(moreToggle);
  const viewerExtras = document.createElement('div');
  viewerExtras.className = 'viewer-extras';
  viewerExtras.hidden = true;
  viewerControls.after(viewerExtras);
  const viewerTools = document.createElement('span');
  viewerTools.className = 'viewer-tools';
  viewerTools.innerHTML = `<button type="button" data-zoom="out" aria-label="缩小 (-)">${svgIcon(strokePath('M6 12h12'))}</button><button type="button" data-view="fit" aria-label="适应窗口 (0)">适屏</button><button type="button" data-view="fill" aria-label="填满窗口">填满</button><button type="button" data-view="actual" aria-label="原始尺寸 (1)">1:1</button><button type="button" data-zoom="in" aria-label="放大 (+)">${svgIcon(strokePath('M12 6v12M6 12h12'))}</button><button type="button" data-info aria-expanded="false" aria-label="图片信息">${svgIcon(strokePath('M12 11v5M12 8h.01') + '<circle cx="12" cy="12" r="9"></circle>')}</button>`;
  const shortcutHelpToggle = document.createElement('button');
  shortcutHelpToggle.type = 'button';
  shortcutHelpToggle.className = 'shortcut-help-toggle';
  shortcutHelpToggle.innerHTML = svgIcon(strokePath('M9.7 9a2.5 2.5 0 1 1 3.9 2.1c-1 .7-1.6 1.2-1.6 2.4M12 17h.01') + '<circle cx="12" cy="12" r="9"></circle>');
  shortcutHelpToggle.title = '查看快捷键 (?)';
  shortcutHelpToggle.setAttribute('aria-label', shortcutHelpToggle.title);
  viewerExtras.append(viewerTools, shortcutHelpToggle);
  const imageInfo = document.createElement('aside');
  imageInfo.className = 'image-info';
  imageInfo.hidden = true;
  const shortcutHelp = document.createElement('aside');
  shortcutHelp.className = 'shortcut-help';
  shortcutHelp.hidden = true;
  shortcutHelp.innerHTML = '<span>← → / J K 切图</span><span>F 点赞 · C 评论 · D / M 标记删除 · Ctrl K 直接删除</span><span>＋ − 缩放 · 0 适屏 · 1 原尺寸</span><span>I 图片信息 · P 轮播 · 空格暂停</span><span>Esc 退出</span>';
  const figure = lightbox.querySelector('figure');
  figure.append(imageInfo, shortcutHelp);
  previewPrevious.innerHTML = svgIcon(strokePath('m15 6-6 6 6 6'));
  previewNext.innerHTML = svgIcon(strokePath('m9 6 6 6-6 6'));
  favouriteToggle.innerHTML = svgIcon(strokePath('M12 20.2 4.8 13a4.7 4.7 0 0 1 6.7-6.6l.5.5.5-.5a4.7 4.7 0 0 1 6.7 6.6L12 20.2Z') + strokePath('m17.2 4.2.45-1.3.45 1.3 1.3.45-1.3.45-.45 1.3-.45-1.3-1.3-.45 1.3-.45Z'));
  deletionToggle.innerHTML = svgIcon(strokePath('m7 7 10 10M17 7 7 17'));
  carouselToggle.innerHTML = svgIcon('<path class="icon-fill" d="m9 7 8 5-8 5Z"></path>' + strokePath('M5 5v14'));
  lightboxClose.innerHTML = svgIcon(strokePath('m6 6 12 12M18 6 6 18'));
  const lightboxState = document.createElement('div');
  lightboxState.className = 'lightbox-state';
  lightboxState.setAttribute('role', 'status');
  lightboxState.innerHTML = `<span class="lightbox-favourite-state">${svgIcon('<path d="M12 21c-.45 0-.85-.18-1.2-.5l-6.7-6.2C1.35 11.75 1.2 7.75 3.7 5.2 6.05 2.8 10 2.85 12 5.45c2-2.6 5.95-2.65 8.3-.25 2.5 2.55 2.35 6.55-.4 9.1l-6.7 6.2c-.35.32-.75.5-1.2.5Z"></path>')}</span><span class="lightbox-deletion-state" hidden>待删除</span>`;
  lightboxStage.append(lightboxState);
  const commentsDrawer = document.createElement('aside');
  commentsDrawer.className = 'gallery-comments';
  commentsDrawer.hidden = true;
  commentsDrawer.setAttribute('aria-label', '图片评论');
  commentsDrawer.innerHTML = '<header><div><span>IMAGE NOTES</span><strong>图片评论</strong></div><button type="button" data-comments-close aria-label="关闭评论区"></button></header><div class="gallery-comments-image"><img alt=""><div><strong></strong><span></span></div><button type="button" data-comment-delete-all hidden>删除所有评论</button></div><div class="gallery-comment-list" role="feed"></div><div class="gallery-comment-empty"><strong>还没有评论</strong><span>记录构图、色彩或需要 AI 调整的细节。</span></div><form class="gallery-comment-composer"><div class="gallery-comment-identity" hidden><span>评论人 <strong></strong></span><button type="button" data-comment-change-author>更换</button></div><label data-comment-author>评论人<input name="author" autocomplete="name" placeholder="你的名字" required></label><label>评论内容<textarea name="body" rows="4" placeholder="例如：压低背景高光，让人物更突出…" required></textarea></label><p class="gallery-comment-hint">Enter 提交 · ⌘ Enter 换行</p><p class="gallery-comment-error" role="status" hidden></p><div><button type="button" data-comment-cancel hidden>取消编辑</button><button type="submit" class="comment-submit">添加评论</button></div></form><footer><a target="_blank">打开共享评论文件</a><span>.gallery-comments.json</span></footer>';
  commentsDrawer.querySelector('[data-comments-close]').innerHTML = svgIcon(strokePath('m7 7 10 10M17 7 7 17'));
  lightbox.append(commentsDrawer);
  const deleteDialog = document.querySelector('#delete-dialog');
  const deleteName = deleteDialog.querySelector('#delete-name');
  const deleteError = deleteDialog.querySelector('#delete-error');
  const deleteCancel = deleteDialog.querySelector('#delete-cancel');
  const deleteConfirm = deleteDialog.querySelector('#delete-confirm');
  const deletionCount = document.querySelector('#deletion-count');
  const deletionFilterLabel = document.querySelector('#deletion-filter-label');
  const deleteMarkedButton = document.querySelector('#delete-marked-images');
  const clearDeletionMarksButton = document.querySelector('#clear-deletion-marks');
  const deleteMarkedDialog = document.querySelector('#delete-marked-dialog');
  const deleteMarkedError = deleteMarkedDialog.querySelector('#delete-marked-error');
  const deleteMarkedCancel = deleteMarkedDialog.querySelector('#delete-marked-cancel');
  const deleteMarkedConfirm = deleteMarkedDialog.querySelector('#delete-marked-confirm');
  const mobileTouch = matchMedia('(hover: none) and (pointer: coarse)');
  const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
  const REVIEW_IDENTITY_KEY = 'http-file-server-review-identity';
  const imageModel = Array.from(listing.querySelectorAll('.entry.image[data-preview-src]'));
  const visibleImages = () => imageModel.filter(entry => entry.isConnected && !entry.hidden);
  let previewTrigger = null;
  let deleting = false;
  let liking = 0;
  let marking = false;
  let moving = false;
  let lastImageTap = null;
  let adjacentPreloads = [];
  let carouselTimer = null;
  let previewRequest = 0;
  let loadingTimer = null;
  let chromeTimer = null;
  let mouseInChromeZone = false;
  let carouselPaused = false;
  let carouselQueue = [];
  let lastCarouselEffect = null;
  let zoom = {scale: 1, x: 0, y: 0, mode: 'fit', originalLoaded: false};
  let pointerGesture = null;
  let galleryComments = {version: 1, comments: []};
  let galleryCommentsLoaded = false;
  let galleryCommentsLoading = false;
  let editingCommentId = null;
  let deleteAllCommentsTimer = null;
  const pendingFavourites = new WeakSet();
  const galleryCommentList = commentsDrawer.querySelector('.gallery-comment-list');
  const galleryCommentEmpty = commentsDrawer.querySelector('.gallery-comment-empty');
  const galleryCommentComposer = commentsDrawer.querySelector('.gallery-comment-composer');
  const galleryCommentAuthor = galleryCommentComposer.elements.author;
  const galleryCommentAuthorLabel = galleryCommentComposer.querySelector('[data-comment-author]');
  const galleryCommentIdentity = galleryCommentComposer.querySelector('.gallery-comment-identity');
  const galleryCommentBody = galleryCommentComposer.elements.body;
  const galleryCommentError = commentsDrawer.querySelector('.gallery-comment-error');
  const galleryCommentSubmit = commentsDrawer.querySelector('.comment-submit');
  const galleryCommentCancel = commentsDrawer.querySelector('[data-comment-cancel]');
  const galleryCommentDeleteAll = commentsDrawer.querySelector('[data-comment-delete-all]');
  const commentCount = commentToggle.querySelector('b');
  const scrollToLatestGalleryComment = (smooth = true) => requestAnimationFrame(() => {
    galleryCommentList.scrollTo({
      top: galleryCommentList.scrollHeight,
      behavior: smooth && !reducedMotion.matches ? 'smooth' : 'auto'
    });
  });
  const syncGalleryCommentIdentity = () => {
    const author = localStorage.getItem(REVIEW_IDENTITY_KEY)?.trim() || '';
    galleryCommentAuthor.value = author;
    galleryCommentAuthorLabel.hidden = Boolean(author);
    galleryCommentIdentity.hidden = !author;
    galleryCommentIdentity.querySelector('strong').textContent = author;
  };
  syncGalleryCommentIdentity();

  const updateLightboxState = () => {
    const liked = previewTrigger?.dataset.favourite === 'true';
    const marked = previewTrigger?.dataset.deletionMarked === 'true';
    lightboxState.querySelector('.lightbox-favourite-state').classList.toggle('liked', liked);
    lightboxState.querySelector('.lightbox-deletion-state').hidden = !marked;
    lightboxState.setAttribute('aria-label', `${liked ? '已点赞' : '未点赞'}${marked ? '，待删除' : ''}`);
  };

  const updateFavouriteButton = () => {
    const liked = previewTrigger?.dataset.favourite === 'true';
    favouriteToggle.setAttribute('aria-pressed', String(liked));
    favouriteToggle.title = liked ? '取消点赞 (f)' : '点赞 (f)';
    favouriteToggle.setAttribute('aria-label', favouriteToggle.title);
    favouriteToggle.disabled = previewTrigger ? pendingFavourites.has(previewTrigger) : false;
    updateLightboxState();
  };

  const markedEntries = () => Array.from(
    listing.querySelectorAll('.entry.image[data-deletion-marked="true"]')
  );

  const updateDeletionControls = () => {
    const marked = previewTrigger?.dataset.deletionMarked === 'true';
    deletionToggle.setAttribute('aria-pressed', String(marked));
    deletionToggle.title = marked ? '取消标记 (m)' : '标记删除 (m)';
    deletionToggle.setAttribute('aria-label', deletionToggle.title);
    deletionToggle.disabled = marking;
    const count = markedEntries().length;
    deletionCount.textContent = String(count);
    deleteMarkedButton.disabled = count === 0 || moving;
    clearDeletionMarksButton.disabled = count === 0 || moving;
    updateLightboxState();
  };

  const updatePreviewButtons = () => {
    const entries = visibleImages();
    const index = entries.indexOf(previewTrigger);
    previewPrevious.disabled = index <= 0;
    previewNext.disabled = index < 0 || index >= entries.length - 1;
    carouselToggle.disabled = entries.length < 2;
    lightboxPosition.textContent = index < 0 ? '' : `${index + 1} / ${entries.length}`;
    const name = previewTrigger?.querySelector('.entry-name').textContent || '';
    lightboxPosition.setAttribute('aria-label', index < 0 ? '' : `${name}，第 ${index + 1} 张，共 ${entries.length} 张`);
  };

  const currentImageName = () => previewTrigger?.querySelector('.entry-name').textContent || '';
  const commentsEndpoint = () => `${previewTrigger.dataset.listHref}?mode=gallery-comments`;
  const formatCommentTime = value => {
    const time = new Date(value);
    return Number.isNaN(time.getTime()) ? value : time.toLocaleString('zh-CN', {dateStyle: 'medium', timeStyle: 'short'});
  };
  const resetCommentComposer = () => {
    editingCommentId = null;
    galleryCommentBody.value = '';
    galleryCommentCancel.hidden = true;
    galleryCommentSubmit.textContent = '添加评论';
    galleryCommentError.hidden = true;
  };
  const renderGalleryComments = () => {
    if (!previewTrigger) return;
    const name = currentImageName();
    const comments = galleryComments.comments.filter(comment => comment.image === name);
    commentCount.textContent = String(comments.length);
    commentCount.hidden = comments.length === 0;
    commentsDrawer.querySelector('.gallery-comments-image img').src = currentThumbnailSource(previewTrigger);
    commentsDrawer.querySelector('.gallery-comments-image strong').textContent = name;
    commentsDrawer.querySelector('.gallery-comments-image span').textContent = `${comments.length} 条评论`;
    galleryCommentDeleteAll.hidden = comments.length === 0;
    galleryCommentDeleteAll.dataset.confirm = 'false';
    galleryCommentDeleteAll.textContent = '删除所有评论';
    commentsDrawer.querySelector('footer a').href = new URL('.gallery-comments.json', location.href).href;
    galleryCommentList.replaceChildren();
    comments.forEach(comment => {
      const article = document.createElement('article');
      article.dataset.commentId = comment.id;
      const header = document.createElement('header');
      const author = document.createElement('strong');
      author.textContent = comment.author;
      const time = document.createElement('time');
      time.dateTime = comment.edited_at || comment.created_at;
      time.textContent = `${formatCommentTime(time.dateTime)}${comment.edited_at ? ' · 已编辑' : ''}`;
      header.append(author, time);
      const body = document.createElement('p');
      body.textContent = comment.body;
      const actions = document.createElement('div');
      actions.innerHTML = '<button type="button" data-comment-edit>编辑</button><button type="button" data-comment-delete>删除</button>';
      article.append(header, body, actions);
      galleryCommentList.append(article);
    });
    galleryCommentEmpty.hidden = comments.length !== 0 || galleryCommentsLoading;
  };
  const loadGalleryComments = async () => {
    if (!previewTrigger || galleryCommentsLoading || galleryCommentsLoaded) return;
    galleryCommentsLoading = true;
    galleryCommentEmpty.hidden = true;
    commentsDrawer.classList.add('loading');
    try {
      const response = await fetch(commentsEndpoint());
      if (!response.ok) throw new Error('评论加载失败，请重试。');
      galleryComments = await response.json();
      galleryCommentsLoaded = true;
      renderGalleryComments();
    } catch (error) {
      galleryCommentError.textContent = error.message;
      galleryCommentError.hidden = false;
    } finally {
      galleryCommentsLoading = false;
      commentsDrawer.classList.remove('loading');
      renderGalleryComments();
    }
  };
  const closeGalleryComments = () => {
    commentsDrawer.hidden = true;
    commentToggle.setAttribute('aria-expanded', 'false');
    resetCommentComposer();
    showChrome();
  };
  const openGalleryComments = () => {
    if (!previewTrigger) return;
    syncGalleryCommentIdentity();
    commentsDrawer.hidden = false;
    commentToggle.setAttribute('aria-expanded', 'true');
    renderGalleryComments();
    loadGalleryComments().then(() => {
      if (!commentsDrawer.hidden) scrollToLatestGalleryComment(false);
    });
    showChrome();
    const input = galleryCommentAuthorLabel.hidden ? galleryCommentBody : galleryCommentAuthor;
    input.focus({preventScroll: true});
  };
  const toggleGalleryComments = () => commentsDrawer.hidden ? openGalleryComments() : closeGalleryComments();
  const submitGalleryCommentAction = async action => {
    galleryCommentSubmit.disabled = true;
    galleryCommentDeleteAll.disabled = true;
    galleryCommentError.hidden = true;
    try {
      const response = await fetch(commentsEndpoint(), {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify(action)
      });
      if (!response.ok) throw new Error((await response.text()).trim() || '保存评论失败，请重试。');
      galleryComments = await response.json();
      galleryCommentsLoaded = true;
      resetCommentComposer();
      renderGalleryComments();
      if (action.type === 'add') scrollToLatestGalleryComment();
    } catch (error) {
      galleryCommentError.textContent = error.message;
      galleryCommentError.hidden = false;
    } finally {
      galleryCommentSubmit.disabled = false;
      galleryCommentDeleteAll.disabled = false;
    }
  };

  const renderFilmstrip = () => {
    const entries = visibleImages();
    const currentIndex = entries.indexOf(previewTrigger);
    lightboxFilmstrip.replaceChildren();
    for (let offset = -3; offset <= 3; offset++) {
      const slot = document.createElement('span');
      slot.className = 'filmstrip-slot';
      slot.dataset.distance = String(Math.abs(offset));
      const entry = entries[currentIndex + offset];
      if (entry) {
        const button = document.createElement('button');
        button.className = 'filmstrip-thumb';
        button.type = 'button';
        button.setAttribute('aria-current', String(offset === 0));
        const name = entry.querySelector('.entry-name').textContent;
        button.setAttribute('aria-label', offset === 0 ? `当前图片：${name}` : `查看 ${name}`);
        const image = document.createElement('img');
        const listingImage = entry.querySelector('.glyph img');
        image.src = listingImage?.dataset.gallerySrc || listingImage?.dataset.listSrc || entry.dataset.previewSrc;
        image.alt = '';
        image.decoding = 'async';
        button.append(image);
        button.addEventListener('click', () => switchPreview(entry, Math.sign(offset)));
        slot.append(button);
      }
      lightboxFilmstrip.append(slot);
    }
  };

  const preloadAdjacentImages = () => {
    const entries = visibleImages();
    const index = entries.indexOf(previewTrigger);
    adjacentPreloads = [entries[index - 1], entries[index + 1]]
      .filter(Boolean)
      .map(entry => {
        const image = new Image();
        image.src = entry.dataset.previewSrc;
        return image;
      });
  };

  const currentThumbnailSource = entry => {
    const image = entry?.querySelector('.glyph img');
    return image?.dataset.gallerySrc || image?.dataset.listSrc || entry?.dataset.previewSrc || '';
  };

  const updateImageInfo = () => {
    if (!previewTrigger) return;
    const name = previewTrigger.querySelector('.entry-name').textContent;
    const extension = name.includes('.') ? name.split('.').pop().toUpperCase() : 'IMAGE';
    const dimensions = lightboxImage.naturalWidth
      ? `${lightboxImage.naturalWidth} × ${lightboxImage.naturalHeight}`
      : '尺寸读取中';
    imageInfo.innerHTML = `<span>${extension}</span><span>${previewTrigger.dataset.fileSize}</span><span>${dimensions}</span><span>${Math.round(zoom.scale * 100)}%</span><a href="${previewTrigger.dataset.originalSrc}" target="_blank">打开原图</a>`;
  };

  const applyZoom = () => {
    lightboxImage.style.transform = `translate3d(calc(${zoom.x}px + var(--gesture-x,0px)),calc(${zoom.y}px + var(--gesture-y,0px)),0) scale(${zoom.scale})`;
    lightboxImage.classList.toggle('zoomed', zoom.scale > 1.01);
    updateImageInfo();
    updateLightboxStatePosition();
  };

  const loadOriginal = async () => {
    if (!previewTrigger) return false;
    if (zoom.originalLoaded || lightboxImage.src === new URL(previewTrigger.dataset.originalSrc, location.href).href) return true;
    const entry = previewTrigger;
    const request = ++previewRequest;
    const original = new Image();
    original.src = entry.dataset.originalSrc;
    try {
      await original.decode();
      if (previewTrigger !== entry || request !== previewRequest) return;
      lightboxImage.src = original.src;
      zoom.originalLoaded = true;
      updateImageInfo();
      return true;
    } catch (_) {
      return false;
    }
  };

  const setZoom = (scale, origin = null) => {
    const previous = zoom.scale;
    zoom.scale = Math.min(8, Math.max(1, scale));
    zoom.mode = zoom.scale === 1 ? 'fit' : 'custom';
    if (origin && previous > 0) {
      const rect = lightboxStage.getBoundingClientRect();
      const ox = origin.x - rect.left - rect.width / 2;
      const oy = origin.y - rect.top - rect.height / 2;
      const ratio = zoom.scale / previous;
      zoom.x = ox - (ox - zoom.x) * ratio;
      zoom.y = oy - (oy - zoom.y) * ratio;
    }
    if (zoom.scale === 1) zoom.x = zoom.y = 0;
    if (zoom.scale > 1.15) loadOriginal();
    applyZoom();
  };

  const fittedImageSize = () => {
    const stage = lightboxStage.getBoundingClientRect();
    const width = lightboxImage.naturalWidth;
    const height = lightboxImage.naturalHeight;
    if (!width || !height) return {width: stage.width, height: stage.height};
    const scale = Math.min(1, stage.width / width, stage.height / height);
    return {width: width * scale, height: height * scale};
  };

  const updateLightboxStatePosition = () => {
    const stage = lightboxStage.getBoundingClientRect();
    const fitted = fittedImageSize();
    const inset = 12;
    lightboxState.style.right = `${Math.max(inset, (stage.width - fitted.width * zoom.scale) / 2 - zoom.x + inset)}px`;
    lightboxState.style.bottom = `${Math.max(inset, (stage.height - fitted.height * zoom.scale) / 2 - zoom.y + inset)}px`;
  };
  addEventListener('resize', updateLightboxStatePosition);

  const setViewMode = async mode => {
    if (!previewTrigger) return;
    if (mode === 'fit') {
      zoom = {scale: 1, x: 0, y: 0, mode: 'fit', originalLoaded: zoom.originalLoaded};
      return applyZoom();
    }
    if (mode === 'actual') {
      if (!await loadOriginal()) return;
      const fitted = fittedImageSize();
      const scale = fitted.width
        ? Math.min(8, Math.max(1, lightboxImage.naturalWidth / fitted.width))
        : 1;
      zoom = {scale, x: 0, y: 0, mode: 'actual', originalLoaded: zoom.originalLoaded};
      return applyZoom();
    }
    const stage = lightboxStage.getBoundingClientRect();
    const fitted = fittedImageSize();
    if (!fitted.width || !fitted.height) return;
    const scale = Math.min(8, Math.max(stage.width / fitted.width, stage.height / fitted.height));
    zoom = {scale, x: 0, y: 0, mode: 'fill', originalLoaded: zoom.originalLoaded};
    applyZoom();
  };

  viewerTools.addEventListener('click', event => {
    const button = event.target.closest('button');
    if (!button) return;
    if (button.dataset.zoom === 'in') setZoom(zoom.scale * 1.25);
    else if (button.dataset.zoom === 'out') setZoom(zoom.scale / 1.25);
    else if (button.dataset.view) setViewMode(button.dataset.view);
    else if (button.hasAttribute('data-info')) {
      imageInfo.hidden = !imageInfo.hidden;
      button.setAttribute('aria-expanded', String(!imageInfo.hidden));
      updateImageInfo();
    }
  });
  shortcutHelpToggle.addEventListener('click', () => {
    shortcutHelp.hidden = !shortcutHelp.hidden;
    shortcutHelpToggle.setAttribute('aria-expanded', String(!shortcutHelp.hidden));
  });
  moreToggle.addEventListener('click', () => {
    viewerExtras.hidden = !viewerExtras.hidden;
    moreToggle.setAttribute('aria-expanded', String(!viewerExtras.hidden));
    moreToggle.classList.toggle('open', !viewerExtras.hidden);
    showChrome();
  });
  commentToggle.addEventListener('click', toggleGalleryComments);
  commentsDrawer.querySelector('[data-comments-close]').addEventListener('click', closeGalleryComments);
  galleryCommentCancel.addEventListener('click', resetCommentComposer);
  galleryCommentAuthor.addEventListener('change', () => {
    const author = galleryCommentAuthor.value.trim();
    if (author) localStorage.setItem(REVIEW_IDENTITY_KEY, author);
    syncGalleryCommentIdentity();
  });
  galleryCommentComposer.querySelector('[data-comment-change-author]').addEventListener('click', () => {
    galleryCommentIdentity.hidden = true;
    galleryCommentAuthorLabel.hidden = false;
    galleryCommentAuthor.focus();
    galleryCommentAuthor.select();
  });
  galleryCommentComposer.addEventListener('submit', event => {
    event.preventDefault();
    const author = galleryCommentAuthor.value.trim();
    const body = galleryCommentBody.value.trim();
    if (!author || !body || !previewTrigger) return;
    localStorage.setItem(REVIEW_IDENTITY_KEY, author);
    syncGalleryCommentIdentity();
    if (editingCommentId) {
      submitGalleryCommentAction({
        type: 'edit',
        comment_id: editingCommentId,
        body,
        edited_at: new Date().toISOString()
      });
      return;
    }
    submitGalleryCommentAction({
      type: 'add',
      comment: {
        id: crypto.randomUUID ? crypto.randomUUID() : `${Date.now()}-${Math.random().toString(16).slice(2)}`,
        image: currentImageName(),
        author,
        body,
        created_at: new Date().toISOString()
      }
    });
  });
  galleryCommentList.addEventListener('click', event => {
    const article = event.target.closest('article[data-comment-id]');
    if (!article) return;
    const comment = galleryComments.comments.find(item => item.id === article.dataset.commentId);
    if (!comment) return;
    if (event.target.closest('[data-comment-edit]')) {
      editingCommentId = comment.id;
      galleryCommentBody.value = comment.body;
      galleryCommentCancel.hidden = false;
      galleryCommentSubmit.textContent = '保存修改';
      galleryCommentBody.focus();
      return;
    }
    const deleteButton = event.target.closest('[data-comment-delete]');
    if (!deleteButton) return;
    if (deleteButton.dataset.confirm !== 'true') {
      deleteButton.dataset.confirm = 'true';
      deleteButton.textContent = '确认删除';
      setTimeout(() => {
        if (!deleteButton.isConnected) return;
        deleteButton.dataset.confirm = 'false';
        deleteButton.textContent = '删除';
      }, 2400);
      return;
    }
    submitGalleryCommentAction({type: 'delete', comment_id: comment.id});
  });
  galleryCommentDeleteAll.addEventListener('click', () => {
    if (galleryCommentDeleteAll.dataset.confirm !== 'true') {
      galleryCommentDeleteAll.dataset.confirm = 'true';
      galleryCommentDeleteAll.textContent = '确认全部删除';
      clearTimeout(deleteAllCommentsTimer);
      deleteAllCommentsTimer = setTimeout(() => {
        galleryCommentDeleteAll.dataset.confirm = 'false';
        galleryCommentDeleteAll.textContent = '删除所有评论';
      }, 2400);
      return;
    }
    clearTimeout(deleteAllCommentsTimer);
    submitGalleryCommentAction({type: 'delete-all'});
  });

  const canAutoHideChrome = () => imageInfo.hidden && shortcutHelp.hidden
    && commentsDrawer.hidden && viewerExtras.hidden && !mouseInChromeZone;
  const showChrome = (hold = false) => {
    lightbox.classList.remove('chrome-hidden');
    clearTimeout(chromeTimer);
    if (!hold && canAutoHideChrome()) {
      chromeTimer = setTimeout(() => lightbox.classList.add('chrome-hidden'), 2500);
    }
  };

  const animateFavouriteFeedback = (liked, origin = null) => {
    favouriteToggle.getAnimations().forEach(animation => animation.cancel());
    favouriteToggle.animate(reducedMotion.matches
      ? [{opacity: .65}, {opacity: 1}]
      : [
          {transform: 'translateY(0) scale(1)'},
          {transform: `translateY(${liked ? -6 : 2}px) scale(${liked ? 1.2 : .9})`, offset: .38},
          {transform: 'translateY(0) scale(1)'}
        ], {duration: reducedMotion.matches ? 160 : 420, easing: 'cubic-bezier(.16,1,.3,1)'});
    if (reducedMotion.matches) return;
    const particleRect = lightbox.getBoundingClientRect();
    const buttonRect = favouriteToggle.getBoundingClientRect();
    const startX = (origin?.x ?? (buttonRect.left + buttonRect.width / 2)) - particleRect.left;
    const startY = (origin?.y ?? (buttonRect.top + buttonRect.height / 2)) - particleRect.top;
    const count = liked ? 9 : 3;
    Array.from(heartParticles.children).forEach((particle, index) => {
      particle.getAnimations().forEach(animation => animation.cancel());
      particle.hidden = index >= count;
      if (particle.hidden) return;
      particle.style.left = `${startX}px`;
      particle.style.top = `${startY}px`;
      particle.classList.toggle('outline', !liked);
      const spread = liked ? 54 + (index % 3) * 18 : 20;
      const angle = liked ? (-150 + index * 18) * Math.PI / 180 : (-110 + index * 20) * Math.PI / 180;
      const x = Math.cos(angle) * spread;
      const y = Math.sin(angle) * spread - (liked ? 34 + (index % 2) * 18 : 4);
      const rotate = -28 + index * 11;
      const scale = liked ? .68 + (index % 4) * .13 : .7;
      const animation = particle.animate([
        {transform: 'translate3d(-50%,-50%,0) scale(.25) rotate(0)', opacity: 0},
        {transform: `translate3d(calc(-50% + ${x * .35}px),calc(-50% + ${y * .25}px),0) scale(${scale * 1.18}) rotate(${rotate * .4}deg)`, opacity: 1, offset: .28},
        {transform: `translate3d(calc(-50% + ${x}px),calc(-50% + ${y}px),0) scale(${scale}) rotate(${rotate}deg)`, opacity: 0}
      ], {duration: liked ? 570 + index * 34 : 360, delay: liked ? index * 24 : index * 35, easing: 'cubic-bezier(.16,1,.3,1)'});
      animation.finished.catch(() => {}).finally(() => { particle.hidden = true; });
    });
  };

  const toggleFavourite = async origin => {
    if (!previewTrigger || lightbox.hidden || deleting || moving || deleteDialog.open) return;
    const entry = previewTrigger;
    if (pendingFavourites.has(entry)) return;
    const liked = entry.dataset.favourite !== 'true';
    liking++;
    pendingFavourites.add(entry);
    favouriteError.hidden = true;
    entry.dataset.favourite = String(liked);
    entry.querySelector('.favourite-mark').hidden = !liked;
    updateFavouriteButton();
    animateFavouriteFeedback(liked, origin);
    try {
      const response = await fetch(`${entry.dataset.listHref}?mode=favourite`, {method: liked ? 'PUT' : 'DELETE'});
      if (!response.ok) throw new Error('保存点赞状态失败，请重试。');
    } catch (error) {
      entry.dataset.favourite = String(!liked);
      entry.querySelector('.favourite-mark').hidden = liked;
      if (previewTrigger === entry) {
        favouriteError.textContent = error.message;
        favouriteError.hidden = false;
      }
    } finally {
      liking--;
      pendingFavourites.delete(entry);
      updateFavouriteButton();
    }
  };
  favouriteToggle.addEventListener('click', toggleFavourite);

  const toggleDeletionMark = async () => {
    if (!previewTrigger || lightbox.hidden || marking || deleting || moving || deleteDialog.open) return;
    const entry = previewTrigger;
    const marked = entry.dataset.deletionMarked !== 'true';
    marking = true;
    deletionMarkError.hidden = true;
    entry.dataset.deletionMarked = String(marked);
    entry.querySelector('.deletion-mark').hidden = !marked;
    updateDeletionControls();
    try {
      const response = await fetch(`${entry.dataset.listHref}?mode=deletion-mark`, {method: marked ? 'PUT' : 'DELETE'});
      if (!response.ok) throw new Error('保存待删除标记失败，请重试。');
    } catch (error) {
      entry.dataset.deletionMarked = String(!marked);
      entry.querySelector('.deletion-mark').hidden = marked;
      if (previewTrigger === entry) {
        deletionMarkError.textContent = error.message;
        deletionMarkError.hidden = false;
      }
    } finally {
      marking = false;
      updateDeletionControls();
    }
  };
  deletionToggle.addEventListener('click', toggleDeletionMark);
  previewPrevious.addEventListener('click', () => stepPreview(-1));
  previewNext.addEventListener('click', () => stepPreview(1));

  const moveDialog = document.createElement('dialog');
  moveDialog.className = 'delete-dialog move-dialog';
  moveDialog.innerHTML = '<h2>整理这些图片？</h2><p class="move-description"></p><div class="delete-actions"><button type="button" data-move-cancel autofocus>取消</button><button type="button" data-move-confirm>确认移动</button></div>';
  document.body.append(moveDialog);
  const moveFavouriteImages = async () => {
    if (moving || liking || marking || deleting) return;
    moving = true;
    moveFavouriteButton.disabled = true;
    directoryNotice.textContent = '正在移入 favourites/…';
    directoryNotice.hidden = false;
    try {
      const url = new URL(location.href);
      url.search = '?mode=move-favourites';
      const response = await fetch(url, {method: 'POST'});
      if (!response.ok) throw new Error(await response.text());
      const result = await response.json();
      let notice = `已将 ${result.moved} 张图片移入 favourites/`;
      if (result.errors.length) notice += `；未完成的操作：${result.errors.join('；')}`;
      history.replaceState({...history.state, moveNotice: notice, previewImage: null}, '');
      location.reload();
    } catch (error) {
      directoryNotice.textContent = error.message || '整理图片失败，请重试。';
      moving = false;
      moveFavouriteButton.disabled = false;
    }
  };
  moveFavouriteButton.addEventListener('click', () => {
    if (moving) return;
    const count = imageModel.filter(entry => entry.dataset.favourite === 'true').length;
    if (count === 0) {
      directoryNotice.textContent = '没有图片需要移动';
      directoryNotice.hidden = false;
      return;
    }
    moveDialog.querySelector('.move-description').textContent = `将 ${count} 张已点赞图片移入 favourites/。同名文件会自动重命名。`;
    moveDialog.showModal();
  });
  moveDialog.querySelector('[data-move-cancel]').addEventListener('click', () => moveDialog.close());
  moveDialog.querySelector('[data-move-confirm]').addEventListener('click', () => {
    moveDialog.close();
    moveFavouriteImages();
  });

  deletionFilter.addEventListener('click', () => {
    const enabled = deletionFilter.getAttribute('aria-pressed') !== 'true';
    deletionFilter.setAttribute('aria-pressed', String(enabled));
    deletionFilterLabel.textContent = enabled ? '查看全部图片' : '查看待删除列表';
    moveFavouriteButton.hidden = enabled;
    deleteMarkedButton.hidden = !enabled;
    clearDeletionMarksButton.hidden = !enabled;
    filterDirectory();
    scheduleThumbnails();
  });

  deleteMarkedButton.addEventListener('click', () => {
    if (moving || markedEntries().length === 0) return;
    deleteMarkedError.hidden = true;
    deleteMarkedDialog.showModal();
  });
  deleteMarkedCancel.addEventListener('click', () => deleteMarkedDialog.close());
  deleteMarkedDialog.addEventListener('cancel', event => {
    if (moving) event.preventDefault();
  });
  clearDeletionMarksButton.addEventListener('click', async () => {
    if (moving || markedEntries().length === 0) return;
    moving = true;
    clearDeletionMarksButton.textContent = '正在取消…';
    moveFavouriteButton.disabled = true;
    updateDeletionControls();
    try {
      const url = new URL(location.href);
      url.search = '?mode=clear-deletion-marks';
      const response = await fetch(url, {method: 'POST'});
      if (!response.ok) throw new Error(await response.text());
      const result = await response.json();
      let notice = `已取消 ${result.cleared} 张图片的删除标记`;
      if (result.errors.length) notice += `；未完成的操作：${result.errors.join('；')}`;
      history.replaceState({...history.state, moveNotice: notice, previewImage: null}, '');
      location.reload();
    } catch (error) {
      moving = false;
      clearDeletionMarksButton.textContent = '取消所有删除标记';
      moveFavouriteButton.disabled = false;
      directoryNotice.textContent = error.message || '取消删除标记失败，请重试。';
      directoryNotice.hidden = false;
      updateDeletionControls();
    }
  });
  deleteMarkedConfirm.addEventListener('click', async () => {
    if (moving) return;
    moving = true;
    deleteMarkedConfirm.disabled = deleteMarkedCancel.disabled = true;
    deleteMarkedConfirm.textContent = '正在删除…';
    deleteMarkedError.hidden = true;
    updateDeletionControls();
    try {
      const url = new URL(location.href);
      url.search = '?mode=delete-marked';
      const response = await fetch(url, {method: 'POST'});
      if (!response.ok) throw new Error(await response.text());
      const result = await response.json();
      let notice = `已删除 ${result.deleted} 张标记图片`;
      if (result.errors.length) notice += `；未完成的操作：${result.errors.join('；')}`;
      history.replaceState({...history.state, moveNotice: notice, previewImage: null}, '');
      location.reload();
    } catch (error) {
      moving = false;
      deleteMarkedConfirm.disabled = deleteMarkedCancel.disabled = false;
      deleteMarkedConfirm.textContent = '全部删除';
      deleteMarkedError.textContent = error.message || '删除已标记图片失败，请重试。';
      deleteMarkedError.hidden = false;
      updateDeletionControls();
    }
  });

  const visibleThumbnails = new Set();
  const loadingThumbnails = new Set();
  const loadedSources = new WeakMap();
  let thumbnailFrame = null;

  const scheduleThumbnails = () => {
    if (thumbnailFrame !== null) return;
    thumbnailFrame = requestAnimationFrame(loadVisibleThumbnails);
  };

  const thumbnailObserver = new IntersectionObserver(entries => {
    entries.forEach(({target, isIntersecting}) => {
      if (isIntersecting) visibleThumbnails.add(target);
      else visibleThumbnails.delete(target);
    });
    scheduleThumbnails();
  }, {rootMargin: '100% 0px'});

  function loadVisibleThumbnails() {
    thumbnailFrame = null;
    const gallery = listing.classList.contains('gallery');
    const candidates = [];
    visibleThumbnails.forEach(image => {
      if (!image.isConnected) {
        visibleThumbnails.delete(image);
        thumbnailObserver.unobserve(image);
        return;
      }
      const source = gallery ? image.dataset.gallerySrc : image.dataset.listSrc;
      if (!source || loadingThumbnails.has(image) || loadedSources.get(image) === source) return;
      const rect = image.getBoundingClientRect();
      if (rect.bottom <= -innerHeight || rect.top >= innerHeight * 2 || rect.right <= 0 || rect.left >= innerWidth) return;
      candidates.push({image, source, distance: Math.abs((rect.top + rect.bottom) / 2 - innerHeight / 2)});
    });
    candidates.sort((left, right) => left.distance - right.distance);
    for (const {image, source} of candidates) {
      if (loadingThumbnails.size >= 4) break;
      loadingThumbnails.add(image);
      const complete = event => {
        image.removeEventListener('load', complete);
        image.removeEventListener('error', complete);
        loadingThumbnails.delete(image);
        if (event.type === 'load') loadedSources.set(image, source);
        else {
          visibleThumbnails.delete(image);
          thumbnailObserver.unobserve(image);
        }
        scheduleThumbnails();
      };
      image.addEventListener('load', complete);
      image.addEventListener('error', complete);
      image.src = source;
    }
  }

  thumbnails.forEach(image => thumbnailObserver.observe(image));

  const carouselEffects = ['drift', 'lift', 'depth', 'soft-focus', 'curtain'];

  const clearCarouselTimer = () => {
    clearTimeout(carouselTimer);
    carouselTimer = null;
  };

  const scheduleCarousel = () => {
    clearCarouselTimer();
    if (!lightbox.classList.contains('carousel-mode') || carouselPaused || lightbox.hidden || document.hidden || deleteDialog.open) return;
    const entries = visibleImages();
    carouselQueue = carouselQueue.filter(entry => entries.includes(entry) && entry !== previewTrigger);
    if (!carouselQueue.length) carouselQueue = shuffle(entries.filter(entry => entry !== previewTrigger));
    if (carouselQueue[0]) {
      const next = new Image();
      next.src = carouselQueue[0].dataset.previewSrc;
      adjacentPreloads.push(next);
    }
    carouselTimer = setTimeout(advanceCarousel, 5000);
  };

  const shuffle = values => {
    for (let index = values.length - 1; index > 0; index--) {
      const next = Math.floor(Math.random() * (index + 1));
      [values[index], values[next]] = [values[next], values[index]];
    }
    return values;
  };

  const advanceCarousel = () => {
    const entries = visibleImages();
    if (entries.length < 2) return scheduleCarousel();
    const currentIndex = entries.indexOf(previewTrigger);
    carouselQueue = carouselQueue.filter(entry => entries.includes(entry) && entry !== previewTrigger);
    if (!carouselQueue.length) carouselQueue = shuffle(entries.filter(entry => entry !== previewTrigger));
    const nextEntry = carouselQueue.shift();
    const nextIndex = entries.indexOf(nextEntry);
    const choices = carouselEffects.filter(effect => effect !== lastCarouselEffect);
    const effect = choices[Math.floor(Math.random() * choices.length)];
    lastCarouselEffect = effect;
    switchPreview(nextEntry, nextIndex > currentIndex ? 1 : -1, effect);
  };

  const showDeleteDialog = () => {
    if (!previewTrigger || deleting || deleteDialog.open) return;
    clearCarouselTimer();
    deleteName.textContent = previewTrigger.querySelector('.entry-name').textContent;
    deleteError.hidden = true;
    deleteDialog.showModal();
  };

  const stepPreview = (direction, gestureOffset = null) => {
    const entries = visibleImages();
    const nextEntry = entries[entries.indexOf(previewTrigger) + direction];
    if (!nextEntry) return false;
    switchPreview(nextEntry, direction, null, gestureOffset);
    return true;
  };

  const fullscreenElement = () => document.fullscreenElement || document.webkitFullscreenElement;
  const enterFullscreen = element => {
    const request = element.requestFullscreen || element.webkitRequestFullscreen;
    return request ? Promise.resolve(request.call(element)) : Promise.reject(new Error('Fullscreen is unavailable'));
  };
  const exitFullscreen = () => {
    const exit = document.exitFullscreen || document.webkitExitFullscreen;
    return exit ? Promise.resolve(exit.call(document)) : Promise.resolve();
  };

  const setGallery = (enabled, updateUrl = true) => {
    listing.classList.toggle('gallery', enabled);
    document.body.classList.toggle('gallery-mode', enabled);
    galleryToggle.setAttribute('aria-pressed', String(enabled));
    galleryToggle.innerHTML = enabled
      ? '<span aria-hidden="true">☷</span> 列表'
      : '<span aria-hidden="true">▦</span> Gallery';
    scheduleThumbnails();
    listing.querySelectorAll('.entry.image[data-gallery-href]').forEach(entry => {
      entry.setAttribute('href', enabled ? entry.dataset.galleryHref : entry.dataset.listHref);
    });
    if (updateUrl) {
      const url = new URL(location.href);
      if (enabled) url.searchParams.set('view', 'gallery');
      else url.searchParams.delete('view');
      history.replaceState(history.state, '', url);
    }
  };

  const setCarousel = async enabled => {
    if (enabled && (lightbox.hidden || carouselToggle.disabled)) return;
    if (enabled && !commentsDrawer.hidden) closeGalleryComments();
    lightbox.classList.toggle('carousel-mode', enabled);
    carouselToggle.setAttribute('aria-pressed', String(enabled));
    carouselToggle.title = enabled ? '退出轮播 (p)' : '进入轮播 (p)';
    carouselToggle.setAttribute('aria-label', carouselToggle.title);
    if (enabled) {
      carouselPaused = false;
      carouselQueue = [];
      document.activeElement?.blur();
      try {
        if (!fullscreenElement()) await enterFullscreen(lightbox);
      } catch (_) {
        const notice = carouselHud.querySelector('.carousel-notice');
        notice.textContent = '已使用网页全屏';
        notice.hidden = false;
        setTimeout(() => { notice.hidden = true; }, 2600);
      }
      showChrome();
      scheduleCarousel();
    } else {
      clearCarouselTimer();
      if (fullscreenElement() === lightbox) exitFullscreen().catch(() => {});
      showChrome();
    }
  };

  carouselToggle.addEventListener('click', () => setCarousel(true));
  carouselHud.addEventListener('click', event => {
    if (event.target.closest('[data-carousel-exit]')) setCarousel(false);
    const pause = event.target.closest('[data-carousel-pause]');
    if (pause) {
      carouselPaused = !carouselPaused;
      pause.textContent = carouselPaused ? '继续' : '暂停';
      if (carouselPaused) clearCarouselTimer(); else scheduleCarousel();
      showChrome();
    }
  });
  const handleFullscreenChange = () => {
    if (!fullscreenElement() && lightbox.classList.contains('carousel-mode')) {
      lightbox.classList.remove('carousel-mode');
      carouselToggle.setAttribute('aria-pressed', 'false');
      carouselToggle.title = '进入轮播 (p)';
      carouselToggle.setAttribute('aria-label', carouselToggle.title);
      carouselPaused = false;
      clearCarouselTimer();
    }
  };
  document.addEventListener('fullscreenchange', handleFullscreenChange);
  document.addEventListener('webkitfullscreenchange', handleFullscreenChange);

  const closeLightbox = () => {
    if (lightbox.hidden || deleting) return;
    clearTimeout(chromeTimer);
    mouseInChromeZone = false;
    setCarousel(false);
    if (deleteDialog.open) deleteDialog.close();
    lightboxStage.getAnimations({subtree: true}).forEach(animation => animation.cancel());
    lightboxStage.querySelectorAll('.preview-outgoing,.backdrop-outgoing').forEach(layer => layer.remove());
    lightbox.hidden = true;
    commentsDrawer.hidden = true;
    commentToggle.setAttribute('aria-expanded', 'false');
    viewerExtras.hidden = true;
    moreToggle.setAttribute('aria-expanded', 'false');
    moreToggle.classList.remove('open');
    previewRequest++;
    clearTimeout(loadingTimer);
    clearTimeout(chromeTimer);
    lightboxImage.removeAttribute('src');
    lightboxPlaceholder.removeAttribute('src');
    lightboxBackdrop.style.backgroundImage = '';
    adjacentPreloads = [];
    document.body.classList.remove('lightbox-open');
    document.querySelector('main').inert = false;
    scrollJumps.inert = false;
    (previewTrigger || galleryToggle).focus();
    previewTrigger = null;
    history.replaceState({...history.state, previewImage: null}, '');
    filterDirectory();
  };

  const animatePreview = (outgoing, outgoingBackdrop, direction, effect, gestureOffset = null, gestureIncoming = null) => {
    lightboxStage.getAnimations({subtree: true}).forEach(animation => animation.cancel());
    lightboxStage.querySelectorAll('.preview-outgoing').forEach(image => {
      if (image !== outgoing) image.remove();
    });
    lightboxStage.querySelectorAll('.backdrop-outgoing').forEach(layer => {
      if (layer !== outgoingBackdrop) layer.remove();
    });
    const vertical = mobileTouch.matches;
    const translate = amount => vertical
      ? `translate3d(0, ${amount}%, 0)`
      : `translate3d(${amount}%, 0, 0)`;
    let outgoingFrames;
    let incomingFrames;
    if (gestureOffset !== null) {
      const axisSize = vertical ? lightboxStage.clientHeight : lightboxStage.clientWidth;
      const translatePixels = amount => vertical
        ? `translate3d(0,${amount}px,0)`
        : `translate3d(${amount}px,0,0)`;
      outgoingFrames = [
        {transform: translatePixels(gestureOffset), opacity: 1},
        {transform: translatePixels(-direction * axisSize), opacity: 1}
      ];
      incomingFrames = [
        {transform: translatePixels(direction * axisSize + gestureOffset), opacity: 1},
        {transform: translatePixels(0), opacity: 1}
      ];
    } else if (reducedMotion.matches) {
      outgoingFrames = [{opacity: 1}, {opacity: 0}];
      incomingFrames = [{opacity: 0}, {opacity: 1}];
    } else if (effect === 'drift') {
      outgoingFrames = [
        {transform: 'translate3d(0,0,0) scale(1)', opacity: 1},
        {transform: `translate3d(${-direction * 9}%,0,0) scale(1.025)`, opacity: 0}
      ];
      incomingFrames = [
        {transform: `translate3d(${direction * 11}%,0,0) scale(.975)`, opacity: 0},
        {transform: 'translate3d(0,0,0) scale(1)', opacity: 1}
      ];
    } else if (effect === 'lift') {
      outgoingFrames = [
        {transform: 'translate3d(0,0,0) scale(1)', opacity: 1},
        {transform: `translate3d(0,${-direction * 10}%,0) scale(.98)`, opacity: 0}
      ];
      incomingFrames = [
        {transform: `translate3d(0,${direction * 13}%,0) scale(1.02)`, opacity: 0},
        {transform: 'translate3d(0,0,0) scale(1)', opacity: 1}
      ];
    } else if (effect === 'depth') {
      outgoingFrames = [
        {transform: 'translate3d(0,0,0) scale(1)', opacity: 1},
        {transform: 'translate3d(0,0,0) scale(1.075)', opacity: 0}
      ];
      incomingFrames = [
        {transform: 'translate3d(0,0,0) scale(.92)', opacity: 0},
        {transform: 'translate3d(0,0,0) scale(1)', opacity: 1}
      ];
    } else if (effect === 'soft-focus') {
      outgoingFrames = [
        {transform: 'translate3d(0,0,0) scale(1)', opacity: 1},
        {transform: `translate3d(${direction * -3}%,0,0) scale(1.035)`, opacity: 0}
      ];
      incomingFrames = [
        {transform: `translate3d(${direction * 3}%,0,0) scale(1.035)`, opacity: 0},
        {transform: 'translate3d(0,0,0) scale(1)', opacity: 1}
      ];
    } else if (effect === 'curtain') {
      outgoingFrames = [
        {transform: 'translate3d(0,0,0) scaleX(1)', opacity: 1},
        {transform: `translate3d(${direction * -7}%,0,0) scaleX(.94)`, opacity: 0}
      ];
      incomingFrames = [
        {transform: `translate3d(${direction * 7}%,0,0) scaleX(.94)`, opacity: 0},
        {transform: 'translate3d(0,0,0) scaleX(1)', opacity: 1}
      ];
    } else {
      outgoingFrames = [
        {transform: translate(0), opacity: 1},
        {transform: translate(-direction * 12), opacity: 0}
      ];
      incomingFrames = [
        {transform: translate(direction * 12), opacity: 0},
        {transform: translate(0), opacity: 1}
      ];
    }
    const carouselTransition = Boolean(effect) && !reducedMotion.matches;
    const gestureTransition = gestureOffset !== null;
    const outgoingAnimation = outgoing.animate(outgoingFrames, {
      duration: gestureTransition ? 230 : reducedMotion.matches ? 130 : carouselTransition ? 720 : 280,
      easing: gestureTransition ? 'cubic-bezier(.22,.75,.28,1)' : 'cubic-bezier(.4,0,1,1)'
    });
    const incomingTarget = gestureIncoming || lightboxImage;
    const incomingAnimation = incomingTarget.animate(incomingFrames, {
      duration: gestureTransition ? 230 : reducedMotion.matches ? 130 : carouselTransition ? 980 : 280,
      easing: 'cubic-bezier(.16,1,.3,1)'
    });
    outgoingAnimation.finished.catch(() => {}).finally(() => outgoing.remove());
    if (gestureIncoming) incomingAnimation.finished.catch(() => {}).finally(() => gestureIncoming.remove());
    if (outgoingBackdrop) {
      const backdropDuration = reducedMotion.matches ? 150 : effect ? 1050 : 320;
      const backdropOptions = {duration: backdropDuration, easing: 'cubic-bezier(.16,1,.3,1)'};
      const outgoingBackdropAnimation = outgoingBackdrop.animate(reducedMotion.matches
        ? [{opacity: .66}, {opacity: 0}]
        : [
            {transform: 'scale(1.08)', opacity: .66},
            {transform: 'scale(1.14)', opacity: 0}
          ], backdropOptions);
      lightboxBackdrop.animate(reducedMotion.matches
        ? [{opacity: 0}, {opacity: .66}]
        : [
            {transform: 'scale(1.14)', opacity: 0},
            {transform: 'scale(1.08)', opacity: .66}
          ], backdropOptions);
      outgoingBackdropAnimation.finished.catch(() => {}).finally(() => outgoingBackdrop.remove());
    }
  };

  const loadPreviewImage = async entry => {
    const request = ++previewRequest;
    clearTimeout(loadingTimer);
    imageLoading.hidden = true;
    imageLoadError.hidden = true;
    lightboxImage.classList.add('loading');
    const load = async source => {
      const pending = new Image();
      pending.src = source;
      await pending.decode();
      return pending;
    };
    loadingTimer = setTimeout(() => {
      if (previewTrigger === entry && request === previewRequest) imageLoading.hidden = false;
    }, 180);
    try {
      let loaded;
      try {
        loaded = await load(entry.dataset.previewSrc);
      } catch (error) {
        if (entry.dataset.previewSrc === entry.dataset.originalSrc) throw error;
        loaded = await load(entry.dataset.originalSrc);
      }
      if (previewTrigger !== entry || request !== previewRequest || lightbox.hidden) return;
      lightboxImage.src = loaded.src;
      lightboxImage.classList.remove('loading');
      lightboxPlaceholder.classList.add('loaded');
      imageLoading.hidden = true;
      zoom.originalLoaded = loaded.src === new URL(entry.dataset.originalSrc, location.href).href;
      updateImageInfo();
      updateLightboxStatePosition();
    } catch (_) {
      if (previewTrigger !== entry || request !== previewRequest) return;
      imageLoading.hidden = true;
      imageLoadError.hidden = false;
    } finally {
      clearTimeout(loadingTimer);
    }
  };

  imageLoadError.querySelector('button').addEventListener('click', () => {
    if (previewTrigger) loadPreviewImage(previewTrigger);
  });

  const showPreview = (entry, direction = 0, effect = null, gestureOffset = null) => {
    lastImageTap = null;
    let outgoing = null;
    let outgoingBackdrop = null;
    let gestureIncoming = null;
    if (!lightbox.hidden && direction) {
      if (gestureOffset !== null) {
        gestureIncoming = lightboxStage.querySelector(`.gesture-neighbour[data-direction="${direction}"]`);
      }
      outgoing = lightboxImage.cloneNode();
      outgoing.classList.add('preview-outgoing');
      outgoing.setAttribute('aria-hidden', 'true');
      lightboxStage.append(outgoing);
      if (lightbox.classList.contains('carousel-mode')) {
        outgoingBackdrop = lightboxBackdrop.cloneNode();
        outgoingBackdrop.classList.add('backdrop-outgoing');
        lightboxStage.append(outgoingBackdrop);
      }
    }
    if (gestureOffset !== null) clearGestureLayers(gestureIncoming);
    const opening = lightbox.hidden;
    previewTrigger = entry;
    lightbox.dataset.filePath = entry.dataset.filePath;
    const name = entry.querySelector('.entry-name').textContent;
    const thumbnailSource = currentThumbnailSource(entry);
    lightboxPlaceholder.src = thumbnailSource;
    lightboxPlaceholder.classList.remove('loaded');
    lightboxBackdrop.style.backgroundImage = `url(${JSON.stringify(thumbnailSource)})`;
    lightboxImage.alt = name;
    lightboxCaption.textContent = name;
    zoom = {scale: 1, x: 0, y: 0, mode: 'fit', originalLoaded: false};
    applyZoom();
    loadPreviewImage(entry);
    favouriteError.hidden = true;
    deletionMarkError.hidden = true;
    updateFavouriteButton();
    updateDeletionControls();
    resetCommentComposer();
    renderGalleryComments();
    loadGalleryComments();
    lightbox.hidden = false;
    document.body.classList.add('lightbox-open');
    document.querySelector('main').inert = true;
    scrollJumps.inert = true;
    if (opening) {
      clearTimeout(chromeTimer);
      mouseInChromeZone = false;
      lightbox.classList.add('chrome-hidden');
      lightboxClose.focus({preventScroll: true});
    }
    history.replaceState({...history.state, previewImage: entry.dataset.listHref}, '');
    updatePreviewButtons();
    renderFilmstrip();
    preloadAdjacentImages();
    if (outgoing) animatePreview(outgoing, outgoingBackdrop, direction, effect, gestureOffset, gestureIncoming);
  };

  const openLightbox = entry => showPreview(entry);

  const switchPreview = (entry, direction, effect = null, gestureOffset = null) => {
    if (!entry || entry === previewTrigger || deleting || deleteDialog.open) return;
    if (effect && !lightbox.classList.contains('carousel-mode')) return;
    showPreview(entry, direction, effect, gestureOffset);
    scheduleCarousel();
  };

  const deleteImage = async () => {
    if (moving || marking || deleting || !previewTrigger || pendingFavourites.has(previewTrigger)) return;
    const entry = previewTrigger;
    deleting = true;
    deleteConfirm.disabled = deleteCancel.disabled = lightboxClose.disabled = true;
    deleteConfirm.textContent = '正在删除…';
    deleteError.hidden = true;
    try {
      const response = await fetch(entry.dataset.listHref, {method: 'DELETE'});
      if (!response.ok) throw new Error('删除失败，请检查文件是否存在且可写后重试。');
      const entries = visibleImages();
      const index = entries.indexOf(entry);
      const nextEntry = entries[index + 1] || entries[index - 1];
      if (deleteDialog.open) deleteDialog.close();
      entry.remove();
      filterDirectory();
      updateDeletionControls();
      deleting = false;
      if (nextEntry) openLightbox(nextEntry);
      else {
        previewTrigger = null;
        closeLightbox();
      }
    } catch (error) {
      if (!deleteDialog.open) showDeleteDialog();
      deleteError.textContent = error.message;
      deleteError.hidden = false;
    } finally {
      deleting = false;
      deleteConfirm.disabled = deleteCancel.disabled = lightboxClose.disabled = false;
      deleteConfirm.textContent = '删除图片';
      if (deleteDialog.open) deleteCancel.focus();
      else if (!lightbox.hidden) lightboxClose.focus();
    }
  };

  deleteConfirm.addEventListener('click', deleteImage);
  deleteCancel.addEventListener('click', () => deleteDialog.close());
  deleteDialog.addEventListener('cancel', event => {
    if (deleting) event.preventDefault();
  });
  deleteDialog.addEventListener('close', scheduleCarousel);

  listing.addEventListener('click', event => {
    const entry = event.target.closest('.entry.image[data-preview-src]');
    if (!listing.classList.contains('gallery') || !entry) return;
    if (!entry) return;
    event.preventDefault();
    openLightbox(entry);
  });

  lightboxClose.addEventListener('click', closeLightbox);
  lightbox.addEventListener('click', event => {
    const button = event.target.closest('button');
    if (button && button !== lightboxClose && !lightbox.hidden) showChrome();
  });
  const updateMouseChrome = event => {
    if (event.pointerType !== 'mouse' || lightbox.hidden) return;
    const controls = figure.querySelector('figcaption').getBoundingClientRect();
    const inChromeZone = event.clientY >= Math.max(0, controls.top - 48);
    if (inChromeZone) {
      mouseInChromeZone = true;
      showChrome(true);
    } else if (mouseInChromeZone) {
      mouseInChromeZone = false;
      showChrome();
    }
  };
  lightbox.addEventListener('pointermove', updateMouseChrome, {passive: true});
  lightbox.addEventListener('pointerleave', event => {
    if (event.pointerType !== 'mouse' || !mouseInChromeZone) return;
    mouseInChromeZone = false;
    showChrome();
  });
  const activePointers = new Map();
  const clearGestureLayers = (preserve = null) => {
    lightboxStage.querySelectorAll('.gesture-neighbour').forEach(image => {
      if (image !== preserve) image.remove();
    });
    lightboxImage.style.removeProperty('--gesture-x');
    lightboxImage.style.removeProperty('--gesture-y');
    lightboxPlaceholder.style.removeProperty('--gesture-x');
    lightboxPlaceholder.style.removeProperty('--gesture-y');
  };
  const gestureNeighbour = direction => {
    const entries = visibleImages();
    const entry = entries[entries.indexOf(previewTrigger) + direction];
    if (!entry) return null;
    let image = lightboxStage.querySelector(`.gesture-neighbour[data-direction="${direction}"]`);
    if (!image) {
      image = document.createElement('img');
      image.className = 'gesture-neighbour';
      image.dataset.direction = direction;
      image.src = currentThumbnailSource(entry);
      image.alt = '';
      image.draggable = false;
      lightboxStage.append(image);
    }
    return image;
  };
  lightbox.addEventListener('pointerdown', event => {
    if (deleting || deleteDialog.open || event.target.closest('button, dialog, a, input, textarea, .gallery-comments') || (event.pointerType === 'mouse' && event.button !== 0)) return;
    activePointers.set(event.pointerId, {x: event.clientX, y: event.clientY});
    lightbox.setPointerCapture?.(event.pointerId);
    if (activePointers.size === 2) {
      const points = Array.from(activePointers.values());
      pointerGesture = {pinch: true, distance: Math.hypot(points[1].x - points[0].x, points[1].y - points[0].y), scale: zoom.scale};
      return;
    }
    pointerGesture = {id: event.pointerId, x: event.clientX, y: event.clientY, lastX: event.clientX, lastY: event.clientY, time: performance.now(), velocity: 0, moved: false, imageTap: event.target === lightboxImage || event.target === lightboxPlaceholder};
  });
  lightbox.addEventListener('pointermove', event => {
    if (!activePointers.has(event.pointerId) || !pointerGesture) return;
    activePointers.set(event.pointerId, {x: event.clientX, y: event.clientY});
    if (activePointers.size >= 2 && pointerGesture.pinch) {
      const points = Array.from(activePointers.values());
      const distance = Math.hypot(points[1].x - points[0].x, points[1].y - points[0].y);
      setZoom(pointerGesture.scale * distance / Math.max(1, pointerGesture.distance));
      event.preventDefault();
      return;
    }
    if (event.pointerId !== pointerGesture.id) return;
    const now = performance.now();
    const dx = event.clientX - pointerGesture.x;
    const dy = event.clientY - pointerGesture.y;
    const delta = mobileTouch.matches ? dy : dx;
    const stepX = event.clientX - pointerGesture.lastX;
    const stepY = event.clientY - pointerGesture.lastY;
    pointerGesture.velocity = (mobileTouch.matches ? stepY : stepX) / Math.max(1, now - pointerGesture.time);
    pointerGesture.lastX = event.clientX;
    pointerGesture.lastY = event.clientY;
    pointerGesture.time = now;
    pointerGesture.moved ||= Math.hypot(dx, dy) > 8;
    if (zoom.scale > 1.01) {
      zoom.x += stepX;
      zoom.y += stepY;
      applyZoom();
    } else if (Math.abs(delta) > Math.abs(mobileTouch.matches ? dx : dy)) {
      const direction = delta < 0 ? 1 : -1;
      const neighbour = gestureNeighbour(direction);
      const resisted = neighbour ? delta : delta * .28;
      const axisSize = mobileTouch.matches ? lightboxStage.clientHeight : lightboxStage.clientWidth;
      const x = mobileTouch.matches ? 0 : resisted;
      const y = mobileTouch.matches ? resisted : 0;
      lightboxImage.style.setProperty('--gesture-x', `${x}px`);
      lightboxImage.style.setProperty('--gesture-y', `${y}px`);
      lightboxPlaceholder.style.setProperty('--gesture-x', `${x}px`);
      lightboxPlaceholder.style.setProperty('--gesture-y', `${y}px`);
      if (neighbour) {
        const origin = direction * axisSize + resisted;
        neighbour.style.transform = mobileTouch.matches ? `translate3d(0,${origin}px,0)` : `translate3d(${origin}px,0,0)`;
      }
    }
    event.preventDefault();
  });
  const finishPointer = event => {
    if (!activePointers.has(event.pointerId)) return;
    activePointers.delete(event.pointerId);
    if (!pointerGesture || pointerGesture.pinch) {
      if (activePointers.size === 0) pointerGesture = null;
      return;
    }
    const gesture = pointerGesture;
    pointerGesture = null;
    const dx = event.clientX - gesture.x;
    const dy = event.clientY - gesture.y;
    const delta = mobileTouch.matches ? dy : dx;
    const threshold = (mobileTouch.matches ? lightboxStage.clientHeight : lightboxStage.clientWidth) * .22;
    if (zoom.scale <= 1.01 && (Math.abs(delta) >= threshold || Math.abs(gesture.velocity) >= .55)) {
      lastImageTap = null;
      if (stepPreview(delta < 0 ? 1 : -1, delta)) return;
    }
    clearGestureLayers();
    applyZoom();
    if (gesture.moved) return;
    showChrome();
    if (!gesture.imageTap) {
      lastImageTap = null;
      return;
    }
    const now = performance.now();
    const doubleTap = lastImageTap && now - lastImageTap.time < 320
      && Math.hypot(event.clientX - lastImageTap.x, event.clientY - lastImageTap.y) < 36;
    if (doubleTap && mobileTouch.matches) {
      clearTimeout(lastImageTap.timer);
      lastImageTap = null;
      toggleFavourite({x: event.clientX, y: event.clientY});
    } else if (doubleTap) {
      clearTimeout(lastImageTap.timer);
      lastImageTap = null;
      setViewMode(zoom.scale > 1.01 ? 'fit' : 'actual');
    } else if (mobileTouch.matches) {
      const tap = {time: now, x: event.clientX, y: event.clientY};
      tap.timer = setTimeout(() => {
        if (lastImageTap !== tap) return;
        lastImageTap = null;
      }, 320);
      lastImageTap = tap;
    } else {
      lastImageTap = null;
    }
  };
  lightbox.addEventListener('pointerup', finishPointer);
  lightbox.addEventListener('pointercancel', event => {
    activePointers.delete(event.pointerId);
    pointerGesture = null;
    clearGestureLayers();
    applyZoom();
  });
  lightboxStage.addEventListener('wheel', event => {
    if (lightbox.hidden) return;
    event.preventDefault();
    setZoom(zoom.scale * Math.exp(-event.deltaY * .0015), {x: event.clientX, y: event.clientY});
    showChrome();
  }, {passive: false});
  document.addEventListener('keydown', event => {
    if (deleting || deleteDialog.open) return;
    const commentEditorActive = !commentsDrawer.hidden
      && (event.target === galleryCommentAuthor || event.target === galleryCommentBody);
    if (commentEditorActive) {
      if (event.isComposing) return;
      if (event.key === 'Escape') {
        event.preventDefault();
        event.target.blur();
      } else if (event.target === galleryCommentAuthor && event.key === 'Enter') {
        event.preventDefault();
        galleryCommentBody.focus();
      } else if (event.target === galleryCommentBody && event.key === 'Enter') {
        event.preventDefault();
        if (event.metaKey) {
          galleryCommentBody.setRangeText('\n', galleryCommentBody.selectionStart, galleryCommentBody.selectionEnd, 'end');
        } else {
          galleryCommentComposer.requestSubmit();
        }
      }
      return;
    }
    if (event.key === 'Escape') {
      if (!commentsDrawer.hidden) closeGalleryComments();
      else if (!viewerExtras.hidden) {
        viewerExtras.hidden = true;
        moreToggle.setAttribute('aria-expanded', 'false');
        moreToggle.classList.remove('open');
      }
      else if (lightbox.classList.contains('carousel-mode')) setCarousel(false);
      else closeLightbox();
      return;
    }
    if (lightbox.hidden) return;
    if (event.key === 'Tab') {
      const focusable = Array.from(lightbox.querySelectorAll('button:not(:disabled),a[href]'))
        .filter(element => element.offsetParent !== null);
      if (!focusable.length) return;
      const index = focusable.indexOf(document.activeElement);
      const next = event.shiftKey
        ? focusable[(index <= 0 ? focusable.length : index) - 1]
        : focusable[(index + 1) % focusable.length];
      event.preventDefault();
      next.focus();
      return;
    }
    if (event.ctrlKey && !event.metaKey && !event.altKey && event.key.toLowerCase() === 'k') {
      event.preventDefault();
      if (!event.repeat) deleteImage();
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey || event.isComposing) {
      return;
    }
    if (event.key.toLowerCase() === 'd') {
      event.preventDefault();
      if (event.repeat) return;
      toggleDeletionMark();
      return;
    }
    if (event.key === '+' || event.key === '=') {
      event.preventDefault();
      setZoom(zoom.scale * 1.25);
      return;
    }
    if (event.key === '-') {
      event.preventDefault();
      setZoom(zoom.scale / 1.25);
      return;
    }
    if (event.key === '0') {
      event.preventDefault();
      setViewMode('fit');
      return;
    }
    if (event.key === '1') {
      event.preventDefault();
      setViewMode('actual');
      return;
    }
    if (event.key.toLowerCase() === 'i') {
      event.preventDefault();
      imageInfo.hidden = !imageInfo.hidden;
      viewerTools.querySelector('[data-info]').setAttribute('aria-expanded', String(!imageInfo.hidden));
      updateImageInfo();
      showChrome();
      return;
    }
    if (event.key === '?') {
      event.preventDefault();
      shortcutHelp.hidden = !shortcutHelp.hidden;
      shortcutHelpToggle.setAttribute('aria-expanded', String(!shortcutHelp.hidden));
      showChrome();
      return;
    }
    if (event.key === ' ' && lightbox.classList.contains('carousel-mode')) {
      event.preventDefault();
      carouselPaused = !carouselPaused;
      carouselHud.querySelector('[data-carousel-pause]').textContent = carouselPaused ? '继续' : '暂停';
      if (carouselPaused) clearCarouselTimer(); else scheduleCarousel();
      showChrome();
      return;
    }
    if (event.key === 'p') {
      if (event.repeat || event.target.closest('input, textarea, select') || event.target.isContentEditable) return;
      event.preventDefault();
      setCarousel(!lightbox.classList.contains('carousel-mode'));
      return;
    }
    if (event.key === 'f') {
      if (event.repeat || event.target.closest('input, textarea, select') || event.target.isContentEditable) return;
      event.preventDefault();
      toggleFavourite();
      return;
    }
    if (event.key.toLowerCase() === 'c') {
      if (event.repeat || event.target.closest('input, textarea, select') || event.target.isContentEditable) return;
      event.preventDefault();
      toggleGalleryComments();
      return;
    }
    if (event.key === 'm') {
      if (event.repeat || event.target.closest('input, textarea, select') || event.target.isContentEditable) return;
      event.preventDefault();
      toggleDeletionMark();
      return;
    }
    let direction;
    if (event.key === 'ArrowUp' || event.key === 'ArrowLeft' || event.key === 'k') direction = -1;
    else if (event.key === 'ArrowDown' || event.key === 'ArrowRight' || event.key === 'j') direction = 1;
    else return;
    event.preventDefault();
    stepPreview(direction);
  });

  document.addEventListener('visibilitychange', () => {
    if (document.hidden) clearCarouselTimer();
    else scheduleCarousel();
  });

  galleryToggle.addEventListener('click', () => {
    if (deleting) return;
    const enabled = !listing.classList.contains('gallery');
    if (!enabled) closeLightbox();
    setGallery(enabled);
  });

  setGallery(new URLSearchParams(location.search).get('view') === 'gallery', false);
  updateDeletionControls();
  if (listing.classList.contains('gallery') && history.state?.previewImage) {
    const entry = visibleImages()
      .find(entry => entry.dataset.listHref === history.state.previewImage);
    if (entry) openLightbox(entry);
  }
}

if (history.state?.scrollY !== undefined) {
  window.scrollTo(history.state.scrollX, history.state.scrollY);
}
"#;

const DIRECTORY_CSS: &str = r#"
:root { color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#202333; --muted:#73788b; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --folder:#6970e8; --grid:#e4e7f0; }
* { box-sizing:border-box; }
[hidden] { display:none !important; }
html { font-size:16px; scrollbar-color:var(--muted) var(--paper); }
body { min-height:100vh; margin:0; color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC","PingFang SC",sans-serif; }
::selection { color:var(--surface); background:var(--accent); }
body.lightbox-open { overflow:hidden; }
.scroll-jumps { position:fixed; z-index:15; top:50%; right:max(.65rem,env(safe-area-inset-right)); display:grid; overflow:hidden; border-radius:.75rem; background:color-mix(in srgb,var(--surface) 88%,transparent); box-shadow:0 10px 30px rgba(27,31,52,.18); backdrop-filter:blur(14px); transform:translateY(-50%); }
.scroll-jumps button { display:grid; place-items:center; width:2.65rem; height:2.65rem; padding:0; border:0; color:var(--muted); background:transparent; cursor:pointer; }
.scroll-jumps button+button { border-top:1px solid var(--line); }
.scroll-jumps button:hover:not(:disabled) { color:var(--accent); background:var(--accent-soft); }
.scroll-jumps button:disabled { opacity:.28; cursor:default; }
.scroll-jumps svg { width:1.15rem; height:1.15rem; fill:none; stroke:currentColor; stroke-width:1.8; stroke-linecap:round; stroke-linejoin:round; }
main { width:min(100% - 2rem,980px); margin:0 auto; padding:clamp(1.5rem,6vw,5rem) 0; }
.gallery-mode main { width:min(100% - 2rem,1320px); }
.breadcrumbs { display:flex; align-items:center; gap:.55rem; overflow-x:auto; padding-bottom:1rem; color:var(--muted); font:600 .78rem/1.4 ui-monospace,SFMono-Regular,Consolas,monospace; scrollbar-width:none; }
.breadcrumbs a { color:inherit; text-decoration:none; white-space:nowrap; }
.breadcrumbs a:hover { color:var(--accent); }
main>header { position:relative; padding:clamp(1.4rem,4vw,2.5rem); overflow:hidden; border:1px solid var(--line); border-radius:1.1rem 1.1rem 0 0; background:var(--surface); }
main>header::after { position:absolute; right:-1.4rem; bottom:-3.2rem; width:9rem; height:7rem; border:1.1rem solid var(--accent-soft); border-radius:1.2rem; content:""; transform:rotate(-8deg); }
.view-toggle { position:absolute; z-index:2; right:clamp(1rem,3vw,2rem); top:clamp(1rem,3vw,2rem); display:flex; align-items:center; gap:.45rem; min-height:2.35rem; padding:.55rem .8rem; border:1px solid var(--line); border-radius:.65rem; color:var(--ink); background:var(--surface); box-shadow:0 6px 18px rgba(54,59,92,.08); font:700 .75rem/1 ui-sans-serif,-apple-system,sans-serif; cursor:pointer; }
.view-toggle:hover,.view-toggle[aria-pressed="true"] { border-color:var(--accent); color:var(--accent); background:var(--accent-soft); }
.view-toggle span { font-size:1rem; }
.eyebrow { margin:0 0 .7rem; color:var(--accent); font:700 .72rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.14em; }
h1 { position:relative; z-index:1; margin:0; overflow-wrap:anywhere; font-family:"Iowan Old Style","Noto Serif SC","Songti SC",Georgia,serif; font-size:clamp(2rem,6vw,3.8rem); line-height:1.08; letter-spacing:-.035em; }
.summary { position:relative; z-index:1; margin:.8rem 0 0; color:var(--muted); font-size:.88rem; }
.directory-search { position:relative; z-index:1; display:block; width:100%; margin-top:1.25rem; padding:.75rem .9rem; border:1px solid var(--line); border-radius:.65rem; color:var(--ink); background:var(--paper); font:inherit; }
.directory-search::placeholder { color:var(--muted); }
.gallery-organise { position:relative; z-index:1; display:flex; flex-wrap:wrap; align-items:center; justify-content:flex-end; gap:.4rem; margin-top:.75rem; }
.organise-label { margin-right:.25rem; color:var(--muted); font-size:.72rem; }
.gallery-organise button { display:inline-flex; align-items:center; gap:.4rem; min-height:2rem; padding:.35rem .6rem; border:1px solid var(--line); border-radius:.45rem; color:var(--muted); background:var(--surface); font:500 .72rem/1.3 ui-sans-serif,-apple-system,sans-serif; cursor:pointer; transition:color .15s ease,border-color .15s ease,background .15s ease; }
.gallery-organise button:hover { border-color:var(--accent); color:var(--ink); background:var(--accent-soft); }
.gallery-organise button:disabled { opacity:.5; cursor:wait; }
.gallery-organise button[aria-pressed="true"] { border-color:var(--accent); color:var(--accent); background:var(--accent-soft); }
.gallery-organise .delete-marked-images:not(:disabled) { border-color:color-mix(in srgb,#b42336 55%,var(--line)); color:light-dark(#a51d31,#ff9ba9); }
.gallery-organise #deletion-count { display:grid; place-items:center; min-width:1.2rem; height:1.2rem; padding:0 .25rem; border-radius:.6rem; color:var(--surface); background:var(--muted); font-size:.65rem; }
.organise-heart { color:light-dark(#bb5268,#dc899b); }
.organise-arrow { opacity:.6; }
.directory-notice { position:relative; z-index:1; margin:.75rem 0 0; color:var(--muted); font-size:.78rem; line-height:1.6; overflow-wrap:anywhere; }
.listing { overflow:hidden; border:1px solid var(--line); border-top:0; border-radius:0 0 1.1rem 1.1rem; background:var(--surface); box-shadow:0 25px 70px rgba(54,59,92,.09); }
.entry { display:grid; grid-template-columns:2.55rem minmax(0,1fr) 5rem 6rem 1.5rem; align-items:center; gap:.85rem; min-height:4.25rem; padding:.7rem 1.2rem; border-top:1px solid var(--line); color:var(--ink); text-decoration:none; transition:background .15s ease,transform .18s cubic-bezier(.16,1,.3,1); }
.entry:first-child { border-top:0; }
.entry:hover { background:var(--accent-soft); transform:translateX(.2rem); }
.entry-name { overflow:hidden; font-weight:650; text-overflow:ellipsis; white-space:nowrap; }
.kind { justify-self:start; padding:.2rem .45rem; border:1px solid var(--line); border-radius:.3rem; color:var(--muted); font:700 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.04em; }
.detail { justify-self:end; color:var(--muted); font:500 .75rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; }
.arrow { color:var(--muted); font-size:1.1rem; opacity:0; transform:translateX(-.35rem); transition:opacity .15s ease,transform .15s ease; }
.entry:hover .arrow { opacity:1; transform:none; }
.glyph { position:relative; justify-self:center; display:block; width:1.4rem; height:1.2rem; border:2px solid var(--muted); border-radius:.18rem; opacity:.75; }
.folder .glyph { height:1rem; margin-top:.2rem; border:0; border-radius:.18rem; background:var(--folder); opacity:1; }
.folder .glyph::before { position:absolute; left:.08rem; top:-.28rem; width:.62rem; height:.38rem; border-radius:.18rem .18rem 0 0; background:var(--folder); content:""; }
.file .glyph::after { position:absolute; right:-2px; top:-2px; width:.42rem; height:.42rem; border-left:2px solid var(--muted); border-bottom:2px solid var(--muted); background:var(--surface); content:""; }
.entry.image .glyph { width:2.55rem; height:2.55rem; overflow:hidden; border:1px solid var(--line); border-radius:.55rem; background-color:var(--surface); background-image:linear-gradient(45deg,var(--grid) 25%,transparent 25%),linear-gradient(-45deg,var(--grid) 25%,transparent 25%),linear-gradient(45deg,transparent 75%,var(--grid) 75%),linear-gradient(-45deg,transparent 75%,var(--grid) 75%); background-position:0 0,0 5px,5px -5px,-5px 0; background-size:10px 10px; box-shadow:inset 0 0 0 1px color-mix(in srgb,var(--line) 65%,transparent); opacity:1; }
.entry.image .glyph::after { content:none; }
.favourite-mark { position:absolute; right:.1rem; bottom:.1rem; display:block; width:1rem; height:1rem; color:#ff4f78; filter:drop-shadow(0 2px 4px rgba(0,0,0,.55)); pointer-events:none; }
.favourite-mark svg { display:block; width:100%; height:100%; overflow:visible; }
.favourite-mark path { fill:currentColor; stroke:rgba(255,255,255,.9); stroke-width:.75; stroke-linejoin:round; }
.gallery .favourite-mark { right:.45rem; bottom:.45rem; width:1.35rem; height:1.35rem; }
.deletion-mark { position:absolute; left:.15rem; top:.15rem; padding:.15rem .3rem; border-radius:.3rem; color:#fff; background:#b42336; box-shadow:0 2px 7px rgba(0,0,0,.25); font:700 .52rem/1.2 ui-sans-serif,-apple-system,sans-serif; white-space:nowrap; pointer-events:none; }
.gallery .deletion-mark { left:.4rem; top:.4rem; padding:.25rem .4rem; font-size:.65rem; }
.entry.image .glyph img { width:100%; height:100%; object-fit:cover; object-position:center; display:block; pointer-events:none; transition:transform .2s ease; }
.entry.image .glyph img:not([src]) { visibility:hidden; }
.entry.image .glyph.thumbnail-error::before { position:absolute; inset:0; display:grid; place-items:center; padding:.5rem; color:var(--muted); content:"预览不可用"; font:600 .62rem/1.35 ui-sans-serif,-apple-system,sans-serif; text-align:center; }
.entry.image:hover .glyph img { transform:scale(1.06); }
.entry.image.vector .glyph img { object-fit:contain; padding:.2rem; }
.gallery-other-heading { display:none; }
.listing.gallery { display:grid; grid-template-columns:repeat(auto-fill,minmax(168px,1fr)); gap:.42rem; padding:.42rem; overflow:visible; background:color-mix(in srgb,var(--surface) 88%,#0b0c12); }
.gallery .entry { grid-template-columns:minmax(0,1fr) auto; grid-template-rows:11rem auto auto; gap:.65rem .75rem; min-width:0; min-height:0; padding:.75rem; overflow:hidden; border:1px solid var(--line); border-radius:.8rem; background:var(--surface); transition:border-color .18s ease,box-shadow .18s ease,transform .18s ease; }
.gallery .entry:first-child { border-top:1px solid var(--line); }
.gallery .entry:hover { padding:.75rem; border-color:color-mix(in srgb,var(--accent) 45%,var(--line)); background:var(--surface); box-shadow:0 14px 30px rgba(54,59,92,.14); transform:translateY(-3px); }
.gallery .glyph { grid-column:1/-1; grid-row:1; align-self:center; }
.gallery .entry.image .glyph { width:100%; height:100%; border-radius:.55rem; }
.gallery .entry-name { grid-column:1/-1; grid-row:2; width:100%; }
.gallery .kind { grid-column:1; grid-row:3; }
.gallery .detail { grid-column:2; grid-row:3; }
.gallery .arrow { display:none; }
.gallery .folder .glyph,.gallery .file:not(.image) .glyph { transform:scale(1.35); }
.gallery .entry.image .glyph { cursor:zoom-in; }
.listing.gallery .entry.folder { order:-1; }
.listing.gallery .entry.image { position:relative; order:0; display:block; min-height:0; aspect-ratio:1; padding:0; overflow:hidden; border:0; border-radius:.7rem; background:#11131a; box-shadow:none; content-visibility:auto; contain-intrinsic-size:180px 180px; transform:none; }
.listing.gallery .entry.image:hover { padding:0; transform:translateY(-2px); }
.listing.gallery .entry.image::after { position:absolute; z-index:1; inset:auto 0 0; height:42%; background:linear-gradient(transparent,rgba(4,5,9,.78)); content:""; opacity:0; transition:opacity .18s ease; pointer-events:none; }
.listing.gallery .entry.image .glyph { position:absolute; inset:0; width:100%; height:100%; border:0; border-radius:inherit; }
.listing.gallery .entry.image .entry-name,.listing.gallery .entry.image .detail { position:absolute; z-index:2; right:.65rem; left:.65rem; color:#fff; opacity:0; transform:translateY(.3rem); transition:opacity .18s ease,transform .18s cubic-bezier(.16,1,.3,1); }
.listing.gallery .entry.image .entry-name { bottom:1.55rem; }
.listing.gallery .entry.image .detail { bottom:.55rem; justify-self:auto; font-size:.68rem; }
.listing.gallery .entry.image:hover::after,.listing.gallery .entry.image:focus-visible::after,.listing.gallery .entry.image:hover .entry-name,.listing.gallery .entry.image:focus-visible .entry-name,.listing.gallery .entry.image:hover .detail,.listing.gallery .entry.image:focus-visible .detail { opacity:1; transform:none; }
.listing.gallery .entry.file:not(.image) { order:2; }
.listing.gallery .gallery-other-heading { display:block; grid-column:1/-1; order:1; margin:1.6rem .55rem .35rem; color:var(--muted); font-size:.78rem; font-weight:700; letter-spacing:.04em; }
.listing.gallery .empty { order:3; }
.delete-dialog { width:min(calc(100% - 2rem),26rem); padding:1.5rem; border:0; border-radius:1rem; color:var(--ink); background:var(--surface); box-shadow:0 24px 80px rgba(0,0,0,.35); }
.delete-dialog::backdrop { background:rgba(10,12,20,.6); }
.delete-dialog h2 { margin:0 0 1rem; font-size:1.3rem; }
.delete-dialog p { margin:.75rem 0; line-height:1.6; }
.delete-name { font-weight:650; overflow-wrap:anywhere; }
.delete-dialog #delete-description { font-size:.875rem; }
.delete-error { color:light-dark(#b42336,#ff9ba9); font-size:.875rem; }
.delete-actions { display:flex; justify-content:flex-end; gap:.75rem; margin-top:1.5rem; }
.delete-actions button { min-height:2.75rem; padding:.65rem 1rem; border:1px solid var(--line); border-radius:.6rem; color:var(--ink); background:var(--surface); font-family:inherit; font-size:.875rem; font-weight:600; line-height:1.2; cursor:pointer; }
.delete-actions button:hover { background:var(--accent-soft); }
.delete-actions #delete-confirm { border-color:transparent; color:#fff; background:#b42336; }
.delete-actions #delete-confirm:hover { background:#941b2b; }
.delete-actions button:disabled { opacity:.6; cursor:wait; }
.image-lightbox { position:fixed; z-index:20; inset:0; display:grid; place-items:center; padding:max(1rem,env(safe-area-inset-top)) max(1rem,env(safe-area-inset-right)) max(1rem,env(safe-area-inset-bottom)) max(1rem,env(safe-area-inset-left)); overflow:hidden; color:#fff; background:rgba(7,8,12,.92); backdrop-filter:blur(18px) saturate(115%); touch-action:none; user-select:none; }
.lightbox-shell { display:grid; grid-template-rows:auto auto minmax(0,1fr); gap:.65rem; width:min(100%,1480px); height:100%; min-height:0; }
.lightbox-position { color:rgba(255,255,255,.72); font:600 .76rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; font-variant-numeric:tabular-nums; letter-spacing:.04em; text-align:center; transition:opacity .22s ease,transform .3s cubic-bezier(.16,1,.3,1); }
.lightbox-filmstrip { display:grid; grid-template-columns:repeat(7,3.5rem); justify-content:center; align-items:center; gap:.4rem; min-height:3.5rem; transition:opacity .22s ease,transform .3s cubic-bezier(.16,1,.3,1); }
.filmstrip-slot { display:grid; place-items:center; width:3.5rem; height:3.5rem; }
.filmstrip-thumb { display:grid; place-items:center; width:3rem; height:2.25rem; padding:0; overflow:hidden; border:1px solid rgba(255,255,255,.18); border-radius:.42rem; background:rgba(255,255,255,.07); box-shadow:0 4px 14px rgba(0,0,0,.22); cursor:pointer; opacity:.66; transform:scale(.94); transition:transform .2s cubic-bezier(.16,1,.3,1),opacity .16s ease,border-color .16s ease,box-shadow .2s ease; }
.filmstrip-thumb img { display:block; width:100%; height:100%; object-fit:cover; }
.filmstrip-thumb:hover { opacity:1; transform:scale(1); }
.filmstrip-thumb[aria-current="true"] { width:3.5rem; height:2.75rem; border-color:rgba(255,255,255,.88); box-shadow:0 7px 20px rgba(0,0,0,.38); opacity:1; transform:scale(1); }
.image-lightbox figure { position:relative; display:grid; grid-template-rows:minmax(0,1fr) auto auto auto; gap:.7rem; min-width:0; min-height:0; margin:0; }
.lightbox-stage { position:relative; display:grid; place-items:center; min-width:0; min-height:0; overflow:hidden; contain:layout paint; }
.carousel-backdrop { position:absolute; z-index:0; inset:-4%; display:none; background-position:center; background-size:cover; opacity:.66; filter:blur(12px) brightness(.4) saturate(1.06); transform:scale(1.08); pointer-events:none; }
.lightbox-stage .lightbox-image { position:absolute; z-index:1; inset:0; display:block; width:100%; height:100%; object-fit:scale-down; filter:drop-shadow(0 18px 32px rgba(0,0,0,.32)); will-change:transform,opacity; transition:opacity .16s ease; }
.lightbox-stage .lightbox-image.loading { opacity:0; }
.lightbox-placeholder,.gesture-neighbour { position:absolute; z-index:1; inset:0; display:block; width:100%; height:100%; object-fit:contain; transform:translate3d(var(--gesture-x,0),var(--gesture-y,0),0); will-change:transform,opacity; }
.lightbox-placeholder { opacity:1; filter:blur(7px) saturate(.9); transform:translate3d(var(--gesture-x,0),var(--gesture-y,0),0) scale(1.015); transition:opacity .2s ease; }
.lightbox-placeholder.loaded { opacity:0; }
.gesture-neighbour { z-index:2; filter:drop-shadow(0 18px 32px rgba(0,0,0,.25)); }
.lightbox-state { position:absolute; z-index:4; display:flex; align-items:center; gap:.42rem; opacity:0; transform:translateY(.3rem); transition:opacity .2s ease,transform .3s cubic-bezier(.16,1,.3,1); pointer-events:none; }
.lightbox-favourite-state { display:none; width:1.2rem; height:1.2rem; color:rgba(255,255,255,.92); filter:drop-shadow(0 2px 4px rgba(0,0,0,.6)); }
.lightbox-favourite-state svg { display:block; width:100%; height:100%; }
.lightbox-favourite-state path { fill:rgba(8,9,14,.28); stroke:currentColor; stroke-width:1.65; stroke-linejoin:round; }
.lightbox-favourite-state.liked { display:block; color:#ff4f78; }
.lightbox-favourite-state.liked path { fill:currentColor; stroke:rgba(255,255,255,.92); stroke-width:.75; }
.lightbox-deletion-state { padding:.15rem .3rem; border-radius:.3rem; color:#fff; background:#b42336; box-shadow:0 2px 7px rgba(0,0,0,.3); font:700 .52rem/1.2 ui-sans-serif,-apple-system,sans-serif; white-space:nowrap; }
.image-loading,.image-load-error { position:absolute; z-index:5; left:50%; bottom:1.25rem; display:flex; align-items:center; gap:.65rem; min-height:2.4rem; padding:.55rem .8rem; border-radius:.7rem; color:rgba(255,255,255,.9); background:rgba(16,18,25,.82); font-size:.75rem; transform:translateX(-50%); backdrop-filter:blur(12px); }
.image-loading i { width:.9rem; height:.9rem; border:2px solid rgba(255,255,255,.25); border-top-color:#fff; border-radius:50%; animation:image-spin .7s linear infinite; }
.image-load-error button { min-height:2rem; padding:.35rem .65rem; border:0; border-radius:.45rem; color:#11131a; background:#fff; font:650 .72rem/1 sans-serif; cursor:pointer; }
@keyframes image-spin { to { transform:rotate(1turn); } }
.heart-particles { position:absolute; z-index:8; inset:0; overflow:visible; pointer-events:none; }
.heart-particles span { position:absolute; display:block; width:clamp(1rem,2.4vw,1.7rem); color:#ff4f78; filter:drop-shadow(0 5px 8px rgba(0,0,0,.3)); will-change:transform,opacity; }
.heart-particles span[hidden] { display:none; }
.heart-particles svg { display:block; width:100%; }
.heart-particles path { fill:currentColor; stroke:rgba(255,255,255,.95); stroke-width:.65; }
.heart-particles span.outline path { fill:rgba(20,22,31,.4); stroke:#ff9bb0; stroke-width:1.4; }
.lightbox-stage .lightbox-image.preview-outgoing { position:absolute; inset:0; width:100%; height:100%; }
.favourite-burst { position:absolute; z-index:2; inset:50% auto auto 50%; width:clamp(5rem,13vw,8rem); color:rgba(255,255,255,.92); pointer-events:none; transform:translate(-50%,-50%); filter:drop-shadow(0 12px 24px rgba(0,0,0,.3)); }
.favourite-burst svg { display:block; width:100%; height:auto; overflow:visible; }
.favourite-burst path { fill:rgba(12,13,18,.42); stroke:currentColor; stroke-width:1.4; stroke-linejoin:round; }
.favourite-burst.liked { color:#ff4f78; }
.favourite-burst.liked path { fill:currentColor; stroke:#fff; stroke-width:.7; }
.image-lightbox figcaption { display:flex; flex-wrap:wrap; align-items:center; justify-content:center; gap:.65rem .75rem; min-width:0; padding:0 .25rem .15rem; font-size:.82rem; transition:opacity .22s ease,transform .3s cubic-bezier(.16,1,.3,1); }
.lightbox-name { min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.lightbox-controls { display:flex; flex-shrink:0; align-items:center; gap:.4rem; }
.preview-step,.deletion-toggle,.carousel-toggle,.comment-toggle,.viewer-more-toggle,.viewer-tools button,.shortcut-help-toggle { position:relative; display:grid; place-items:center; min-width:2.75rem; height:2.75rem; padding:0 .7rem; border:1px solid rgba(255,255,255,.26); border-radius:.8rem; color:#fff; background:rgba(255,255,255,.045); font:650 .72rem/1 sans-serif; cursor:pointer; transition:background .18s ease,border-color .18s ease,transform .18s cubic-bezier(.16,1,.3,1); }
.preview-step,.deletion-toggle,.carousel-toggle,.comment-toggle,.viewer-more-toggle { width:2.75rem; padding:0; }
.preview-step svg,.deletion-toggle svg,.carousel-toggle svg,.comment-toggle svg,.viewer-more-toggle svg,.viewer-tools svg,.shortcut-help-toggle svg,.lightbox-close svg,.gallery-comments button svg { width:1.15rem; fill:none; stroke:currentColor; stroke-width:1.7; stroke-linecap:round; stroke-linejoin:round; }
.carousel-toggle .icon-fill { fill:currentColor; stroke:none; }
.preview-step:hover,.deletion-toggle:hover,.carousel-toggle:hover,.comment-toggle:hover,.viewer-more-toggle:hover,.viewer-tools button:hover,.shortcut-help-toggle:hover { background:rgba(255,255,255,.12); transform:translateY(-1px); }
.preview-step:disabled,.deletion-toggle:disabled,.carousel-toggle:disabled { opacity:.35; cursor:default; }
.deletion-toggle[aria-pressed="true"] { border-color:#ff9ba9; color:#fff; background:#b42336; }
.carousel-toggle[aria-pressed="true"] { border-color:rgba(255,255,255,.82); background:rgba(255,255,255,.16); }
.comment-toggle[aria-expanded="true"] { border-color:#b9b5ff; color:#d5d2ff; background:rgba(91,91,214,.25); }
.comment-toggle b { position:absolute; top:-.35rem; right:-.35rem; display:grid; place-items:center; min-width:1.15rem; height:1.15rem; padding:0 .25rem; border:2px solid #11131a; border-radius:999px; color:#11131a; background:#d5d2ff; font:750 .6rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; }
.viewer-more-toggle svg { transition:transform .24s cubic-bezier(.16,1,.3,1); }
.viewer-more-toggle.open svg { transform:rotate(180deg); }
.favourite-toggle { flex-shrink:0; display:grid; place-items:center; width:3rem; height:3rem; padding:0; border:1px solid rgba(255,255,255,.3); border-radius:50%; color:#fff; background:rgba(255,255,255,.045); cursor:pointer; }
.favourite-toggle svg { width:1.5rem; fill:transparent; stroke:currentColor; stroke-width:1.45; transition:fill .18s ease,color .18s ease; }
.favourite-toggle:hover { background:rgba(255,255,255,.1); }
.favourite-toggle[aria-pressed="true"] { color:#ff6685; }
.favourite-toggle[aria-pressed="true"] svg { fill:currentColor; }
.favourite-toggle:disabled { opacity:.5; cursor:wait; }
.lightbox-error { margin:0; color:#ff9ba9; font-size:.82rem; text-align:center; }
.lightbox-close { position:fixed; z-index:9; top:max(1rem,env(safe-area-inset-top)); right:max(1rem,env(safe-area-inset-right)); display:grid; place-items:center; width:2.75rem; height:2.75rem; padding:0; border:0; color:rgba(255,255,255,.78); background:transparent; cursor:pointer; transition:color .18s ease,opacity .22s ease,transform .3s cubic-bezier(.16,1,.3,1); }
.lightbox-close svg { width:1.35rem; }
.lightbox-close:hover { color:#fff; transform:scale(1.08); }
.viewer-extras { flex-basis:100%; display:flex; justify-content:center; align-items:center; gap:.35rem; animation:viewer-extras-in .24s cubic-bezier(.16,1,.3,1); }
@keyframes viewer-extras-in { from { opacity:0; transform:translateY(-.35rem); } }
.viewer-tools { display:flex; gap:.3rem; }
.viewer-tools button[data-view="fit"],.viewer-tools button[data-view="actual"] { min-width:3.2rem; }
.image-info,.shortcut-help { position:absolute; z-index:7; right:.5rem; bottom:4.2rem; display:flex; flex-wrap:wrap; align-items:center; gap:.65rem; max-width:min(92vw,36rem); padding:.75rem .9rem; border:1px solid rgba(255,255,255,.16); border-radius:.8rem; color:rgba(255,255,255,.82); background:rgba(14,16,23,.88); box-shadow:0 14px 44px rgba(0,0,0,.3); backdrop-filter:blur(16px); font-size:.72rem; }
.image-info a { color:#fff; text-underline-offset:.2em; }
.shortcut-help { left:50%; right:auto; justify-content:center; transform:translateX(-50%); }
.gallery-comments { position:fixed; z-index:12; top:max(1rem,env(safe-area-inset-top)); right:max(1rem,env(safe-area-inset-right)); bottom:max(1rem,env(safe-area-inset-bottom)); display:grid; grid-template-columns:minmax(0,1fr); grid-template-rows:auto auto minmax(0,1fr) auto auto; width:min(25rem,calc(100vw - 2rem)); overflow:hidden; border:1px solid rgba(255,255,255,.18); border-radius:1.1rem; color:#f5f5fa; background:rgba(17,19,27,.96); box-shadow:0 28px 90px rgba(0,0,0,.52); backdrop-filter:blur(24px) saturate(125%); animation:gallery-comments-in .34s cubic-bezier(.16,1,.3,1); }
@keyframes gallery-comments-in { from { opacity:0; transform:translateX(1.5rem) scale(.985); } }
.gallery-comments>header { grid-area:1/1; display:flex; align-items:center; justify-content:space-between; padding:1.1rem 1.15rem .9rem; border-bottom:1px solid rgba(255,255,255,.1); }
.gallery-comments>header div { display:grid; gap:.25rem; }
.gallery-comments>header span { color:#aaa6ff; font:750 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.13em; }
.gallery-comments>header strong { font:700 1rem/1.2 ui-sans-serif,-apple-system,sans-serif; }
.gallery-comments>header button { display:grid; place-items:center; width:2.5rem; height:2.5rem; padding:0; border:1px solid rgba(255,255,255,.14); border-radius:.75rem; color:#fff; background:transparent; cursor:pointer; }
.gallery-comments-image { grid-area:2/1; display:grid; grid-template-columns:3.4rem minmax(0,1fr) auto; align-items:center; gap:.8rem; padding:.85rem 1.15rem; border-bottom:1px solid rgba(255,255,255,.08); background:rgba(255,255,255,.025); }
.gallery-comments-image img { width:3.4rem; height:3.4rem; border-radius:.65rem; object-fit:cover; background:#090a0f; }
.gallery-comments-image div { display:grid; min-width:0; gap:.3rem; }
.gallery-comments-image strong { overflow:hidden; font-size:.82rem; text-overflow:ellipsis; white-space:nowrap; }
.gallery-comments-image span { color:rgba(255,255,255,.55); font-size:.7rem; }
.gallery-comments-image [data-comment-delete-all] { padding:.4rem 0; border:0; color:rgba(255,143,162,.78); background:transparent; font:650 .66rem/1.2 sans-serif; cursor:pointer; }
.gallery-comments-image [data-comment-delete-all]:hover,.gallery-comments-image [data-comment-delete-all][data-confirm="true"] { color:#ff9bae; }
.gallery-comments-image [data-comment-delete-all]:disabled { opacity:.45; cursor:wait; }
.gallery-comment-list { grid-area:3/1; min-height:0; overflow:auto; padding:0 1.15rem; overscroll-behavior:contain; scrollbar-color:rgba(255,255,255,.24) transparent; }
.gallery-comment-list article { padding:1rem 0; border-bottom:1px solid rgba(255,255,255,.09); }
.gallery-comment-list article>header { display:flex; align-items:baseline; justify-content:space-between; gap:.75rem; }
.gallery-comment-list article strong { color:#fff; font-size:.76rem; }
.gallery-comment-list article time { color:rgba(255,255,255,.42); font:500 .62rem/1.3 ui-monospace,SFMono-Regular,Consolas,monospace; text-align:right; }
.gallery-comment-list article p { margin:.55rem 0 .7rem; color:rgba(255,255,255,.8); font-size:.82rem; line-height:1.65; white-space:pre-wrap; overflow-wrap:anywhere; user-select:text; }
.gallery-comment-list article>div { display:flex; gap:.75rem; }
.gallery-comment-list article button,.gallery-comment-composer [data-comment-cancel] { padding:0; border:0; color:rgba(255,255,255,.5); background:transparent; font:650 .68rem/1 sans-serif; cursor:pointer; }
.gallery-comment-list article button:hover,.gallery-comment-composer [data-comment-cancel]:hover { color:#fff; }
.gallery-comment-list [data-comment-delete][data-confirm="true"] { color:#ff8fa2; }
.gallery-comment-empty { grid-area:3/1; place-self:center; display:grid; justify-items:center; gap:.4rem; padding:2rem; color:rgba(255,255,255,.5); text-align:center; pointer-events:none; }
.gallery-comment-empty strong { color:rgba(255,255,255,.78); font-size:.86rem; }
.gallery-comment-empty span { max-width:18rem; font-size:.72rem; line-height:1.55; }
.gallery-comments.loading .gallery-comment-empty { display:none; }
.gallery-comment-composer { grid-area:4/1; min-height:0; max-height:min(52dvh,23rem); display:grid; gap:.7rem; overflow-y:auto; padding:1rem 1.15rem; border-top:1px solid rgba(255,255,255,.1); background:rgba(8,9,14,.46); overscroll-behavior:contain; scrollbar-color:rgba(255,255,255,.24) transparent; }
.gallery-comment-identity { display:flex; align-items:center; justify-content:space-between; gap:.75rem; color:rgba(255,255,255,.52); font:650 .68rem/1.3 sans-serif; }
.gallery-comment-identity strong { color:rgba(255,255,255,.9); }
.gallery-comment-identity button { padding:.25rem 0; border:0; color:#b9b5ff; background:transparent; font:650 .68rem/1 sans-serif; cursor:pointer; }
.gallery-comment-composer label { display:grid; gap:.35rem; color:rgba(255,255,255,.5); font:650 .65rem/1 sans-serif; }
.gallery-comment-composer input,.gallery-comment-composer textarea { width:100%; border:1px solid rgba(255,255,255,.14); border-radius:.7rem; color:#fff; background:rgba(255,255,255,.055); font:500 .8rem/1.55 ui-sans-serif,-apple-system,"Noto Sans SC",sans-serif; caret-color:#b9b5ff; outline:none; }
.gallery-comment-composer input { height:2.5rem; padding:0 .75rem; }
.gallery-comment-composer textarea { min-height:5.7rem; max-height:12rem; padding:.65rem .75rem; resize:vertical; }
.gallery-comment-composer input:focus,.gallery-comment-composer textarea:focus { border-color:#aaa6ff; box-shadow:0 0 0 3px rgba(170,166,255,.12); }
.gallery-comment-hint { margin:-.25rem 0 0; color:rgba(255,255,255,.38); font:600 .64rem/1.4 ui-monospace,SFMono-Regular,Consolas,monospace; }
.gallery-comment-composer>div { display:flex; align-items:center; justify-content:flex-end; gap:.9rem; }
.gallery-comment-composer .comment-submit { min-height:2.5rem; padding:.65rem 1rem; border:0; border-radius:.7rem; color:#11131a; background:#d5d2ff; font:750 .72rem/1 sans-serif; cursor:pointer; }
.gallery-comment-composer .comment-submit:disabled { opacity:.55; cursor:wait; }
.gallery-comment-error { margin:0; color:#ff9bad; font-size:.7rem; }
.gallery-comments>footer { grid-area:5/1; display:flex; align-items:center; justify-content:space-between; gap:.75rem; padding:.7rem 1.15rem; border-top:1px solid rgba(255,255,255,.08); color:rgba(255,255,255,.35); font:550 .62rem/1.4 ui-monospace,SFMono-Regular,Consolas,monospace; }
.gallery-comments>footer a { color:#aaa6ff; text-underline-offset:.2em; }
.carousel-hud { position:fixed; z-index:10; left:50%; bottom:max(1.25rem,env(safe-area-inset-bottom)); display:none; gap:.45rem; transform:translateX(-50%); transition:opacity .22s ease,transform .3s cubic-bezier(.16,1,.3,1); }
.carousel-hud button { min-height:2.75rem; padding:.65rem .9rem; border:1px solid rgba(255,255,255,.25); border-radius:.75rem; color:#fff; background:rgba(12,14,20,.72); backdrop-filter:blur(12px); cursor:pointer; }
.carousel-notice { align-self:center; padding:.55rem .7rem; border-radius:.65rem; color:rgba(255,255,255,.78); background:rgba(12,14,20,.72); font-size:.72rem; backdrop-filter:blur(12px); }
.image-lightbox.chrome-hidden figcaption { opacity:0; pointer-events:none; transform:translateY(.55rem); }
.image-lightbox:not(.carousel-mode) .lightbox-state { opacity:1; transform:none; }
.image-lightbox.carousel-mode { padding:0; background:#000; backdrop-filter:none; }
.image-lightbox:fullscreen { width:100vw; height:100vh; height:100dvh; inset:0; }
.image-lightbox:-webkit-full-screen { width:100vw; height:100vh; height:100dvh; inset:0; }
.image-lightbox.carousel-mode .lightbox-close,.image-lightbox.carousel-mode .lightbox-position,.image-lightbox.carousel-mode .lightbox-filmstrip,.image-lightbox.carousel-mode figcaption,.image-lightbox.carousel-mode .lightbox-state,.image-lightbox.carousel-mode .lightbox-error,.image-lightbox.carousel-mode .image-info,.image-lightbox.carousel-mode .shortcut-help,.image-lightbox.carousel-mode .gallery-comments { display:none; }
.image-lightbox.carousel-mode .carousel-hud { display:flex; }
.image-lightbox.carousel-mode .lightbox-shell { grid-template-rows:minmax(0,1fr); width:100%; height:100%; }
.image-lightbox.carousel-mode figure { grid-template-rows:minmax(0,1fr); }
.image-lightbox.carousel-mode .carousel-backdrop { display:block; }
.image-lightbox.carousel-mode .lightbox-stage .lightbox-image,.image-lightbox.carousel-mode .lightbox-placeholder { width:100%; height:100%; max-width:100vw; max-height:100vh; max-height:100dvh; object-fit:contain; filter:none; }
.empty { display:grid; grid-column:1/-1; place-items:center; min-height:14rem; color:var(--muted); }
.empty span { font:300 3rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; }
.empty p { margin:.8rem 0 0; }
:focus-visible { outline:3px solid color-mix(in srgb,var(--accent) 55%,transparent); outline-offset:-3px; }
@media (max-width:650px) { main,.gallery-mode main { width:100%; padding:1rem; } main>header { padding:1.5rem 1.1rem; } .view-toggle { position:relative; right:auto; top:auto; width:max-content; margin-top:1rem; } .entry { grid-template-columns:2.4rem minmax(0,1fr) auto; padding-inline:1rem; } .kind,.arrow { display:none; } .detail { grid-column:3; } .entry.image .glyph { width:2.4rem; height:2.4rem; } .listing.gallery { grid-template-columns:repeat(auto-fill,minmax(145px,1fr)); gap:.65rem; padding:.65rem; } .gallery .entry { grid-template-columns:minmax(0,1fr); grid-template-rows:8.5rem auto auto; padding:.6rem; } .gallery .entry:hover { padding:.6rem; } .gallery .entry.image .glyph { width:100%; height:100%; } .gallery .detail { grid-column:1; grid-row:3; justify-self:start; } .scroll-jumps { right:max(.4rem,env(safe-area-inset-right)); } .scroll-jumps button { width:2.8rem; height:2.8rem; } .image-lightbox { padding:max(.7rem,env(safe-area-inset-top)) max(.7rem,env(safe-area-inset-right)) max(.7rem,env(safe-area-inset-bottom)) max(.7rem,env(safe-area-inset-left)); } .lightbox-shell { gap:.45rem; } .lightbox-filmstrip { grid-template-columns:repeat(5,3.35rem); gap:.25rem; } .filmstrip-slot { width:3.35rem; } .filmstrip-slot[data-distance="3"] { display:none; } .image-lightbox figcaption { flex-wrap:wrap; } .lightbox-name { width:100%; text-align:center; } .lightbox-controls { gap:.25rem; } .preview-step,.deletion-toggle,.carousel-toggle,.comment-toggle,.viewer-more-toggle { min-width:2.8rem; width:2.8rem; height:2.8rem; } .favourite-toggle { width:2.9rem; height:2.9rem; } .gallery-comments { inset:auto max(.45rem,env(safe-area-inset-right)) max(.45rem,env(safe-area-inset-bottom)) max(.45rem,env(safe-area-inset-left)); width:auto; height:min(92dvh,42rem); border-radius:1.15rem; animation-name:gallery-comments-mobile-in; } @keyframes gallery-comments-mobile-in { from { opacity:0; transform:translateY(1.5rem) scale(.985); } } .gallery-comment-composer textarea { min-height:4.8rem; } }
@media (max-height:620px) { .gallery-comments>header { padding:.7rem .9rem .6rem; } .gallery-comments>header button { width:2.15rem; height:2.15rem; } .gallery-comments-image { grid-template-columns:2.7rem minmax(0,1fr) auto; gap:.65rem; padding:.55rem .9rem; } .gallery-comments-image img { width:2.7rem; height:2.7rem; } .gallery-comment-empty { gap:.25rem; padding:.75rem; } .gallery-comment-composer { max-height:58dvh; gap:.5rem; padding:.7rem .9rem; } .gallery-comment-composer textarea { min-height:4rem; } .gallery-comments>footer { display:none; } }
@media (max-width:1024px),(hover:none) and (pointer:coarse) { .preview-step,.deletion-toggle,.carousel-toggle,.comment-toggle,.viewer-more-toggle,.viewer-tools button,.shortcut-help-toggle { min-width:3rem; height:3rem; } .lightbox-controls { width:100%; min-width:0; flex-shrink:1; justify-content:center; flex-wrap:wrap; } .viewer-extras,.viewer-tools { width:100%; justify-content:center; border:0; } .image-info,.shortcut-help { left:50%; right:auto; bottom:8.7rem; width:max-content; max-width:calc(100vw - 2rem); transform:translateX(-50%); } }
@media (hover:none) and (pointer:coarse) { .listing.gallery .entry.image,.listing.gallery .entry.image:hover { display:block; padding:0; } .listing.gallery .entry.image::after,.listing.gallery .entry.image .entry-name,.listing.gallery .entry.image .detail { opacity:1; transform:none; } .image-lightbox figcaption { padding-bottom:max(.2rem,env(safe-area-inset-bottom)); } }
@media (prefers-color-scheme:dark) { :root { --paper:#11131b; --surface:#191c27; --ink:#edf0f7; --muted:#a7adbd; --line:#303545; --accent:#a9a5ff; --accent-soft:#292943; --folder:#8e8af5; --grid:#262b38; } body { background-image:radial-gradient(circle at 50% -20%,#252943 0,transparent 38rem); } .listing { box-shadow:0 25px 70px rgba(0,0,0,.25); } }
@media (prefers-reduced-motion:reduce) { .entry,.arrow,.entry.image .glyph img,.filmstrip-thumb { transition:none; } .entry.image:hover .glyph img,.gallery .entry:hover,.filmstrip-thumb { transform:none; } }
"#;

const MARKDOWN_CSS: &str = r#"
:root { --file-header-padding:max(1rem,calc((100vw - 1120px)/2)); color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#202333; --muted:#6d7287; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --code:#171925; --code-ink:#e8eaf2; --quote:#eef4ff; --comment:#fff1b8; --comment-line:#c89926; --addressed:#9b6b22; --success:#2f7d55; }
* { box-sizing:border-box; }
[hidden] { display:none !important; }
html { font-size:17px; scroll-padding-top:7.4rem; }
body { margin:0; padding-top:calc(3.4rem + 1px); color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC","PingFang SC",sans-serif; line-height:1.78; }
.topbar { position:fixed; inset:0 0 auto; z-index:4; display:flex; align-items:center; gap:.75rem; min-height:3.4rem; padding:.7rem var(--file-header-padding); border-bottom:1px solid color-mix(in srgb,var(--line) 75%,transparent); background:color-mix(in srgb,var(--paper) 88%,transparent); backdrop-filter:blur(16px); }
.home { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.5rem; color:var(--muted); text-decoration:none; }
.home:hover { color:var(--accent); background:var(--accent-soft); }
.mark { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.55rem; color:white; background:var(--accent); font:700 .68rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:-.04em; transform:rotate(-3deg); }
.filename { overflow:hidden; color:var(--muted); font:600 .8rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; }
.top-actions { display:flex; align-items:center; gap:.55rem; margin-left:auto; }
.button,.format-tools button { border:1px solid var(--line); border-radius:.5rem; color:white; background:var(--accent); font:650 .78rem/1 ui-sans-serif,-apple-system,sans-serif; cursor:pointer; }
.button { display:inline-grid; place-items:center; min-height:2rem; padding:.5rem .8rem; text-decoration:none; }
.button.ghost { color:var(--ink); background:var(--surface); }
.button.danger { border-color:#bd3e52; background:#bd3e52; }
.button:hover,.format-tools button:hover { filter:brightness(.96); transform:translateY(-1px); }
.button:active,.format-tools button:active { transform:translateY(1px); }
.identity-button { max-width:11rem; overflow:hidden; padding:.4rem .6rem; border:0; color:var(--muted); background:transparent; font:600 .72rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; cursor:pointer; }
.identity-button:hover { color:var(--accent); }
.review-toggle { display:inline-flex; align-items:center; gap:.3rem; }
.review-toggle b { min-width:1.15rem; padding:.12rem .3rem; border-radius:.3rem; color:var(--accent); background:var(--accent-soft); font:700 .64rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; font-variant-numeric:tabular-nums; }
.reader-layout { display:grid; grid-template-areas:"toc paper review"; grid-template-columns:260px minmax(0,1100px) minmax(280px,340px); gap:1.5rem; width:min(100% - 2rem,1760px); margin:clamp(1.25rem,4vw,3.5rem) 1rem; }
body.review-closed .reader-layout { grid-template-areas:"toc paper"; grid-template-columns:260px minmax(0,1100px); width:min(100% - 2rem,1400px); }
body.review-closed .review-panel { display:none; }
main.paper { width:100%; margin:0; padding:clamp(1.25rem,5vw,4.6rem) clamp(1.25rem,3vw,3rem); border:1px solid var(--line); border-radius:1.1rem; background:var(--surface); box-shadow:0 24px 70px rgba(54,59,92,.09); }
#toc { grid-area:toc; position:sticky; top:7.4rem; align-self:start; max-height:calc(100vh - 9.4rem); overflow:auto; padding:.35rem; font-size:.74rem; }
main.paper { grid-area:paper; }
#toc:empty { display:none; }
#toc::before { display:block; margin:0 0 .7rem .55rem; color:var(--muted); content:"ON THIS PAGE"; font:700 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.1em; }
#toc a { display:block; width:max-content; min-width:100%; padding:.35rem .55rem; border-left:1px solid var(--line); color:var(--muted); text-decoration:none; white-space:nowrap; }
#toc a[data-level="3"] { padding-left:1.15rem; }
#toc a:hover { border-color:var(--accent); color:var(--accent); }
article { max-width:1000px; margin:auto; }
h1,h2,h3,h4,h5,h6 { color:var(--ink); font-family:"Iowan Old Style","Noto Serif SC","Songti SC",Georgia,serif; line-height:1.25; letter-spacing:-.025em; text-wrap:balance; }
h1 { margin:0 0 1.4rem; font-size:clamp(2.15rem,7vw,4rem); }
h2 { margin:2.8rem 0 .9rem; padding-bottom:.45rem; border-bottom:1px solid var(--line); font-size:1.75rem; }
h3 { margin:2rem 0 .7rem; font-size:1.3rem; }
p,ul,ol,blockquote,pre,table { margin:0 0 1.25rem; }
a { color:var(--accent); text-decoration-thickness:.08em; text-underline-offset:.18em; }
a:hover { text-decoration-thickness:.15em; }
strong { color:var(--ink); }
blockquote { margin-left:0; padding:.9rem 1.1rem; border-left:4px solid var(--accent); border-radius:0 .65rem .65rem 0; color:var(--muted); background:var(--quote); }
blockquote > :last-child { margin-bottom:0; }
code { padding:.12rem .35rem; border:1px solid var(--line); border-radius:.35rem; color:#b33d72; background:var(--accent-soft); font:.88em/1.5 ui-monospace,SFMono-Regular,Consolas,monospace; }
pre { position:relative; overflow:auto; padding:1.2rem 1.35rem; border-radius:.8rem; background:var(--code); box-shadow:inset 0 1px rgba(255,255,255,.08); }
pre code { padding:0; border:0; color:var(--code-ink); background:transparent; font-size:.84rem; }
pre.mermaid-rendered { padding:2rem 1.5rem; background:var(--surface); border:1px solid var(--line); box-shadow:0 4px 18px rgba(40,46,75,.035); }
pre.mermaid-rendered > code { display:none; }
.mermaid-diagram { white-space:normal; text-align:center; line-height:1.5; }
.mermaid-diagram p { margin:0; }
.mermaid-diagram svg { display:block; max-width:100%; height:auto; margin:auto; }
.mermaid-error { margin-top:.75rem; color:var(--code-ink); white-space:pre-wrap; font:.8rem/1.5 ui-monospace,monospace; }
.copy-code { position:absolute; top:.55rem; right:.55rem; padding:.32rem .5rem; border:1px solid #34384a; border-radius:.35rem; color:#aeb4c7; background:#222533; font:600 .65rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; cursor:pointer; opacity:0; }
pre:hover .copy-code,.copy-code:focus { opacity:1; }
table { display:block; overflow-x:auto; width:100%; border-spacing:0; border-collapse:collapse; }
th,td { padding:.65rem .8rem; border:1px solid var(--line); text-align:left; }
th { background:var(--accent-soft); font-size:.86rem; }
img { display:block; max-width:100%; height:auto; margin:1.75rem auto; border-radius:.65rem; }
hr { margin:2.5rem 0; border:0; border-top:1px solid var(--line); }
li + li { margin-top:.25rem; }
input[type="checkbox"] { width:1rem; height:1rem; margin-right:.45rem; accent-color:var(--accent); }
sup { line-height:0; }
:focus-visible { outline:3px solid color-mix(in srgb,var(--accent) 55%,transparent); outline-offset:3px; border-radius:.2rem; }
.source-run { border-radius:.15rem; transition:background .18s ease,box-shadow .18s ease; }
.source-run.has-review { background:color-mix(in srgb,var(--comment) 72%,transparent); box-shadow:0 0 0 2px color-mix(in srgb,var(--comment) 52%,transparent); cursor:pointer; }
.source-run.has-addressed-review { background:color-mix(in srgb,var(--accent-soft) 68%,transparent); box-shadow:0 0 0 2px color-mix(in srgb,var(--accent-soft) 52%,transparent); cursor:pointer; }
.source-run.review-target { background:color-mix(in srgb,var(--comment) 92%,var(--surface)); box-shadow:0 0 0 4px color-mix(in srgb,var(--comment-line) 38%,transparent); }
.review-panel { grid-area:review; position:sticky; top:7.05rem; align-self:start; display:flex; flex-direction:column; max-height:calc(100vh - 8.4rem); max-height:calc(100dvh - 8.4rem); overflow:hidden; border:1px solid var(--line); border-radius:.9rem; background:var(--surface); box-shadow:0 18px 50px rgba(54,59,92,.08); }
.review-header { display:flex; align-items:center; justify-content:space-between; padding:1rem 1rem .75rem; }
.review-header div { display:grid; gap:.32rem; }
.review-header span,.dialog-kicker { color:var(--accent); font:750 .6rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.13em; }
.review-header strong { font:650 1rem/1.2 ui-sans-serif,-apple-system,sans-serif; }
.review-header button { display:none; width:2rem; height:2rem; padding:0; border:0; color:var(--muted); background:transparent; font-size:1.35rem; cursor:pointer; }
.review-file-actions { display:flex; flex-wrap:wrap; gap:.4rem; padding:0 1rem .75rem; }
.review-file-actions a,.review-file-actions button { padding:.35rem .5rem; border:1px solid var(--line); border-radius:.35rem; color:var(--muted); background:var(--surface); font:500 .7rem/1.4 ui-sans-serif,-apple-system,sans-serif; text-decoration:none; cursor:pointer; }
.review-file-actions a:hover,.review-file-actions button:hover { color:var(--accent); border-color:var(--accent); background:var(--accent-soft); }
.review-presence { display:flex; align-items:flex-start; gap:.5rem; min-height:2.15rem; padding:.55rem 1rem; border-block:1px solid var(--line); color:var(--muted); background:var(--paper); font-size:.7rem; line-height:1.45; }
.review-presence i { flex:0 0 auto; width:.45rem; height:.45rem; margin-top:.25rem; border-radius:50%; background:var(--success); box-shadow:0 0 0 .18rem color-mix(in srgb,var(--success) 14%,transparent); }
.document-comment { margin:.8rem 1rem 0; padding:.62rem .75rem; border:1px dashed color-mix(in srgb,var(--accent) 42%,var(--line)); border-radius:.55rem; color:var(--accent); background:var(--accent-soft); font:650 .75rem/1 ui-sans-serif,-apple-system,sans-serif; text-align:left; cursor:pointer; }
.document-comment:hover { border-style:solid; }
.review-composer { margin:.8rem 1rem 0; padding:.8rem; border-left:3px solid var(--accent); background:var(--paper); }
.review-composer p { max-height:4.5rem; overflow:auto; margin:0 0 .6rem; color:var(--muted); font:.68rem/1.45 ui-monospace,SFMono-Regular,Consolas,monospace; white-space:pre-wrap; }
.review-composer label { display:block; margin-bottom:.42rem; color:var(--ink); font-size:.72rem; font-weight:650; }
.review-composer textarea,.reply-box textarea { width:100%; resize:vertical; padding:.65rem; border:1px solid var(--line); border-radius:.45rem; color:var(--ink); background:var(--surface); font:500 .78rem/1.55 ui-sans-serif,-apple-system,sans-serif; }
.review-composer > div,.reply-box > div { display:flex; justify-content:flex-end; gap:.45rem; margin-top:.5rem; }
.text-button { padding:.45rem .55rem; border:0; color:var(--muted); background:transparent; font:650 .72rem/1 ui-sans-serif,-apple-system,sans-serif; cursor:pointer; }
.text-button:hover { color:var(--accent); }
.review-filters { display:grid; grid-template-columns:repeat(3,1fr); margin-top:.8rem; padding:0 1rem; border-bottom:1px solid var(--line); }
.review-filters button { display:flex; justify-content:center; gap:.25rem; padding:.65rem .2rem; border:0; border-bottom:2px solid transparent; color:var(--muted); background:transparent; font:600 .68rem/1 ui-sans-serif,-apple-system,sans-serif; cursor:pointer; }
.review-filters button.active { border-bottom-color:var(--accent); color:var(--accent); }
.review-filters b { font-variant-numeric:tabular-nums; }
.review-complete { display:grid; gap:.2rem; margin:.85rem 1rem 0; padding:.75rem; border-left:3px solid var(--success); color:var(--success); background:color-mix(in srgb,var(--success) 8%,var(--surface)); }
.review-complete strong { font-size:.78rem; }
.review-complete span { font-size:.7rem; }
.review-error { margin:.8rem 1rem 0; padding:.7rem; border-left:3px solid #bd3e52; color:#bd3e52; background:color-mix(in srgb,#bd3e52 8%,var(--surface)); font-size:.72rem; }
.comment-list { flex:1; overflow:auto; padding:.85rem 1rem 1rem; }
.comment-empty { padding:2.5rem .75rem; color:var(--muted); font-size:.76rem; text-align:center; }
.comment-card { padding:.85rem 0 1rem; border-bottom:1px solid var(--line); scroll-margin-top:1rem; }
.comment-card:first-child { padding-top:0; }
.comment-card:last-child { border-bottom:0; }
.comment-card.active { margin-inline:-.55rem; padding-inline:.55rem; background:var(--accent-soft); }
.comment-meta { display:flex; align-items:center; justify-content:space-between; gap:.5rem; margin-bottom:.55rem; }
.comment-status { color:var(--comment-line); font:750 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.05em; }
.comment-status.addressed { color:var(--addressed); }
.comment-status.resolved { color:var(--success); }
.comment-scope { overflow:hidden; margin:0 0 .65rem; padding:.48rem .6rem; border-left:2px solid var(--comment-line); color:var(--muted); background:color-mix(in srgb,var(--comment) 42%,var(--surface)); font:.68rem/1.5 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:pre-wrap; }
.comment-scope.stale { border-left-color:#bd3e52; color:#a83a4c; background:color-mix(in srgb,#bd3e52 7%,var(--surface)); }
.message { display:grid; gap:.28rem; margin-top:.7rem; }
.message header { display:flex; justify-content:space-between; gap:.5rem; color:var(--muted); font-size:.63rem; }
.message header strong { overflow:hidden; color:var(--ink); font:650 .67rem/1.3 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; }
.message p { margin:0; color:var(--ink); font-size:.76rem; line-height:1.55; white-space:pre-wrap; overflow-wrap:anywhere; }
.message-tools { display:flex; gap:.2rem; margin-top:.18rem; }
.message-tools button { padding:.25rem .3rem; border:0; color:var(--muted); background:transparent; font:600 .62rem/1 ui-sans-serif,-apple-system,sans-serif; cursor:pointer; }
.message-tools button:hover { color:var(--accent); }
.message-tools button[data-confirming="true"] { color:#bd3e52; }
.message-editor { margin-top:.45rem; }
.message-editor textarea { width:100%; resize:vertical; padding:.55rem .6rem; border:1px solid var(--line); border-radius:.4rem; color:var(--ink); background:var(--surface); font:500 .75rem/1.5 ui-sans-serif,-apple-system,sans-serif; }
.message-editor div { display:flex; justify-content:flex-end; gap:.35rem; margin-top:.4rem; }
.comment-actions { display:flex; flex-wrap:wrap; gap:.35rem; margin-top:.8rem; }
.comment-actions button { padding:.4rem .52rem; border:1px solid var(--line); border-radius:.4rem; color:var(--muted); background:var(--surface); font:650 .67rem/1 ui-sans-serif,-apple-system,sans-serif; cursor:pointer; }
.comment-actions button.primary { border-color:var(--accent); color:white; background:var(--accent); }
.comment-actions button[data-confirming="true"] { border-color:#bd3e52; color:#bd3e52; }
.reply-box { margin-top:.65rem; }
.selection-comment { position:fixed; z-index:8; padding:.55rem .7rem; border:1px solid color-mix(in srgb,var(--accent) 55%,var(--line)); border-radius:.5rem; color:white; background:var(--accent); box-shadow:0 10px 28px rgba(54,59,92,.22); font:650 .72rem/1 ui-sans-serif,-apple-system,sans-serif; cursor:pointer; transform:translate(-50%,-100%); }
.toast { position:fixed; z-index:10; right:1rem; bottom:1rem; max-width:min(24rem,calc(100vw - 2rem)); padding:.7rem .9rem; border:1px solid var(--line); border-radius:.55rem; color:var(--ink); background:var(--surface); box-shadow:0 16px 45px rgba(54,59,92,.18); font-size:.75rem; }
.identity-dialog { width:min(92vw,430px); padding:0; border:1px solid var(--line); border-radius:1rem; color:var(--ink); background:var(--surface); box-shadow:0 28px 90px rgba(35,38,61,.24); }
.identity-dialog::backdrop { background:rgba(20,22,34,.52); backdrop-filter:blur(5px); }
.identity-dialog form { display:grid; gap:.8rem; padding:1.5rem; }
.identity-dialog h2 { margin:0; padding:0; border:0; font:650 1.35rem/1.25 ui-sans-serif,-apple-system,sans-serif; }
.identity-dialog p { margin:0; color:var(--muted); font-size:.78rem; line-height:1.6; }
.identity-dialog label { margin-top:.25rem; font-size:.72rem; font-weight:650; }
.identity-dialog input { width:100%; padding:.72rem .8rem; border:1px solid var(--line); border-radius:.5rem; color:var(--ink); background:var(--paper); font:550 .82rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; }
.identity-error { min-height:1em; color:#bd3e52; font-size:.68rem; }
.editor-shell { display:flex; flex-direction:column; height:calc(100vh - 5.8rem); height:calc(100dvh - 5.8rem); overflow:hidden; background:var(--surface); }
.editor-toolbar { display:flex; flex:0 0 auto; align-items:center; gap:.6rem; min-height:3.5rem; padding:.6rem max(1rem,calc((100vw - 1400px)/2)); border-bottom:1px solid var(--line); }
.format-tools { display:flex; gap:.3rem; overflow-x:auto; }
.format-tools button { flex:0 0 auto; width:2rem; height:2rem; padding:0; color:var(--ink); background:var(--paper); }
.collaboration-status { display:flex; align-items:center; gap:.42rem; margin-left:auto; color:var(--muted); font-size:.75rem; white-space:nowrap; }
#connection-dot { width:.5rem; height:.5rem; border-radius:50%; background:#9aa0b3; box-shadow:0 0 0 .2rem color-mix(in srgb,#9aa0b3 15%,transparent); }
#connection-dot.connected { background:#3b9b67; box-shadow:0 0 0 .2rem color-mix(in srgb,#3b9b67 16%,transparent); }
#connection-dot.error { background:#bd3e52; box-shadow:0 0 0 .2rem color-mix(in srgb,#bd3e52 16%,transparent); }
#presence::before { content:"·"; margin-right:.42rem; }
.editor-panes { display:grid; flex:1 1 auto; grid-template-columns:minmax(0,1fr) minmax(0,1fr); min-height:0; }
.pane { display:grid; grid-template-rows:2rem 1fr; min-width:0; min-height:0; margin:0; }
.pane > span { padding:.7rem 1rem; color:var(--muted); background:var(--paper); font:700 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.12em; }
.preview-pane { border-right:1px solid var(--line); }
#source { width:100%; height:100%; resize:none; padding:1.25rem clamp(1rem,3vw,2rem); border:0; outline:0; color:var(--ink); background:var(--surface); font:500 .9rem/1.75 ui-monospace,SFMono-Regular,Consolas,"Noto Sans Mono CJK SC",monospace; tab-size:2; }
#source:disabled { color:var(--muted); cursor:wait; }
#preview { width:100%; height:100%; border:0; background:var(--paper); }
.preview-body { min-height:100vh; }
main.preview-paper { width:100%; margin:0; padding:2rem; border:0; border-radius:0; box-shadow:none; }
.sync-anchor { display:block; overflow:hidden; width:0; height:0; pointer-events:none; }
body.editing { overflow:hidden; }
@media (max-width:1180px) { .reader-layout { grid-template-areas:"paper review"; grid-template-columns:minmax(0,820px) minmax(280px,340px); width:min(100% - 2rem,1180px); } body.review-closed .reader-layout { grid-template-areas:"paper"; grid-template-columns:minmax(0,1100px); width:min(100% - 2rem,1100px); } #toc { display:none; } }
@media (max-width:900px) { .reader-layout,body.review-closed .reader-layout { display:block; width:min(100% - 2rem,820px); } .review-panel { position:fixed; z-index:7; top:calc(5.8rem + 1px); right:0; bottom:0; width:min(92vw,370px); max-height:none; border-radius:0; transform:translateX(0); transition:transform .22s ease; } body.review-closed .review-panel { display:flex; transform:translateX(100%); pointer-events:none; } .review-header button { display:block; } }
@media (max-width:700px) { html { font-size:16px; } .reader-layout,body.review-closed .reader-layout { width:100%; margin:0; } main.paper { padding:1.5rem 1rem 3rem; border-width:0; border-radius:0; box-shadow:none; } :root { --file-header-padding:.7rem; } .mark,.identity-button,.raw-button { display:none; } .editor-panes { grid-template-columns:1fr; grid-template-rows:1fr 1fr; } .preview-pane { border-right:0; border-bottom:1px solid var(--line); } .collaboration-status { margin-left:auto; } #save-status { display:none; } }
@media (hover:none) and (pointer:coarse) { .selection-comment { top:auto !important; bottom:max(1rem,calc(env(safe-area-inset-bottom) + .5rem)); left:50% !important; min-height:2.75rem; padding:.7rem 1rem; transform:translateX(-50%); } }
@media (prefers-color-scheme:dark) { :root { --paper:#11131b; --surface:#191c27; --ink:#edf0f7; --muted:#a7adbd; --line:#303545; --accent:#a9a5ff; --accent-soft:#292943; --code:#0d0f16; --code-ink:#e7e9f3; --quote:#20283a; --comment:#5d4b20; --comment-line:#d9ad43; --addressed:#e0ae63; --success:#6fc394; } body { background-image:radial-gradient(circle at 50% -20%,#252943 0,transparent 38rem); } code { color:#f2a7ca; } main { box-shadow:0 24px 70px rgba(0,0,0,.25); } }
@media (prefers-reduced-motion:no-preference) { main.paper { animation:arrive .35s ease-out both; } @keyframes arrive { from { opacity:0; transform:translateY(8px); } } }
"#;

const MARKDOWN_JS: &str = r#"
const reader = document.querySelector('#reader');
const editor = document.querySelector('#editor');
const source = document.querySelector('#source');
const preview = document.querySelector('#preview');
const status = document.querySelector('#save-status');
const presence = document.querySelector('#presence');
const connectionDot = document.querySelector('#connection-dot');
const article = document.querySelector('#article');
const reviewPanel = document.querySelector('#review-panel');
const commentList = document.querySelector('#comment-list');
const reviewError = document.querySelector('#review-error');
const reviewUsers = document.querySelector('#review-users');
const selectionComment = document.querySelector('#selection-comment');
const reviewComposer = document.querySelector('#review-composer');
const composerScope = document.querySelector('#composer-scope');
const commentBody = document.querySelector('#comment-body');
const identityDialog = document.querySelector('#identity-dialog');
const identityInput = document.querySelector('#identity-input');
const identityError = document.querySelector('#identity-error');
const identityButton = document.querySelector('#identity-button');
const overlayReviewLayout = matchMedia('(max-width:900px)');
const touchReviewDevice = matchMedia('(hover: none) and (pointer: coarse)');
const REVIEW_IDENTITY_KEY = 'http-file-server-review-identity';
commentBody.placeholder = 'Enter 提交 · ⌘ Enter 换行';
const LOCAL_ORIGIN = Symbol('local-input');
const REMOTE_ORIGIN = Symbol('remote-update');
let documentState = new Y.Doc();
let sharedText = documentState.getText('source');
let socket, reconnectTimer, previewTimer;
let roomId = null;
let reconnectDelay = 400;
let previewRevision = 0;
let scrollSyncLocked = false;
let sourceScrollMap;
let scrollMapFrame;
let scrollAnchors = [];
let previewResizeObserver;
let editing = false;
let synced = false;
let exitRequested = false;
let sequence = 0;
let latestGeneration = 0;
let savedGeneration = 0;
const pending = new Set();
let reviewDocument = {version: 1, comments: []};
let reviewFilter = 'open';
let reviewIdentity = '';
let reviewSocket;
let reviewReconnectTimer;
let reviewReconnectDelay = 500;
let pendingCommentScope = null;
let pendingSelectionLabel = '';
let selectedCommentId = null;
let pendingIdentityAction = null;

function setStatus(message, state = '') {
  status.textContent = message;
  connectionDot.className = state;
}
function refreshStatus() {
  if (!socket || socket.readyState !== WebSocket.OPEN) setStatus('连接已断开', 'error');
  else if (!synced || pending.size) setStatus('正在同步…');
  else if (savedGeneration < latestGeneration) setStatus('等待自动保存', 'connected');
  else setStatus('已保存', 'connected');
}
function scrollRatio(top, height, clientHeight) {
  const available = height - clientHeight;
  return available > 0 ? top / available : 0;
}
function withScrollLock(callback) {
  if (scrollSyncLocked) return;
  scrollSyncLocked = true;
  callback();
  requestAnimationFrame(() => { scrollSyncLocked = false; });
}
function documentHeight(doc) {
  return Math.max(doc.documentElement.scrollHeight, doc.body.scrollHeight);
}
function interpolateScroll(value, from, to) {
  if (scrollAnchors.length < 2) return null;
  const points = scrollAnchors.slice().sort((left, right) => left[from] - right[from]);
  if (value <= points[0][from]) return points[0][to];
  for (let index = 1; index < points.length; index += 1) {
    const previous = points[index - 1];
    const next = points[index];
    if (value > next[from]) continue;
    const distance = next[from] - previous[from];
    if (distance <= 0) return next[to];
    return previous[to] + (next[to] - previous[to]) * (value - previous[from]) / distance;
  }
  return points[points.length - 1][to];
}
function rebuildScrollMap() {
  const win = preview.contentWindow;
  const doc = preview.contentDocument;
  if (!win || !doc?.body || !source.clientWidth) return;
  const previewAnchors = Array.from(doc.querySelectorAll('.sync-anchor[data-source-offset]'));
  const offsets = [...new Set(previewAnchors.map(anchor => Number(anchor.dataset.sourceOffset)))]
    .filter(offset => Number.isFinite(offset) && offset >= 0 && offset <= source.value.length)
    .sort((left, right) => left - right);

  sourceScrollMap ||= document.body.appendChild(document.createElement('div'));
  sourceScrollMap.setAttribute('aria-hidden', 'true');
  const style = getComputedStyle(source);
  Object.assign(sourceScrollMap.style, {
    position: 'fixed', visibility: 'hidden', pointerEvents: 'none', left: '-100000px', top: '0',
    width: `${source.clientWidth}px`, height: 'auto', boxSizing: 'border-box', border: '0',
    padding: style.padding, font: style.font, lineHeight: style.lineHeight,
    letterSpacing: style.letterSpacing, whiteSpace: 'pre-wrap', overflowWrap: 'break-word',
    wordBreak: style.wordBreak, tabSize: style.tabSize
  });
  sourceScrollMap.replaceChildren();
  const sourceMarkers = new Map();
  let cursor = 0;
  offsets.forEach(offset => {
    sourceScrollMap.append(document.createTextNode(source.value.slice(cursor, offset)));
    const marker = document.createElement('i');
    marker.style.cssText = 'display:inline-block;width:0;height:0;overflow:hidden';
    sourceScrollMap.append(marker);
    sourceMarkers.set(offset, marker);
    cursor = offset;
  });
  sourceScrollMap.append(document.createTextNode(source.value.slice(cursor) + '\u200b'));
  const mirrorTop = sourceScrollMap.getBoundingClientRect().top;
  const uniqueAnchors = new Map();
  previewAnchors.forEach(anchor => {
    const offset = Number(anchor.dataset.sourceOffset);
    if (uniqueAnchors.has(offset) || !sourceMarkers.has(offset)) return;
    uniqueAnchors.set(offset, {
      source: sourceMarkers.get(offset).getBoundingClientRect().top - mirrorTop,
      preview: anchor.getBoundingClientRect().top + win.scrollY
    });
  });
  scrollAnchors = [
    {source: 0, preview: 0},
    ...uniqueAnchors.values(),
    {source: source.scrollHeight, preview: documentHeight(doc)}
  ];
}
function scheduleScrollMapRebuild(syncAfter = true) {
  cancelAnimationFrame(scrollMapFrame);
  scrollMapFrame = requestAnimationFrame(() => {
    rebuildScrollMap();
    if (syncAfter) syncPreviewFromSource();
  });
}
function syncPreviewFromSource() {
  const win = preview.contentWindow;
  const doc = preview.contentDocument;
  if (!win || !doc?.body) return;
  const sourceCenter = source.scrollTop + source.clientHeight / 2;
  const mappedCenter = interpolateScroll(sourceCenter, 'source', 'preview');
  const sourceMax = Math.max(0, source.scrollHeight - source.clientHeight);
  const previewMax = Math.max(0, documentHeight(doc) - win.innerHeight);
  const fallback = scrollRatio(source.scrollTop, source.scrollHeight, source.clientHeight) * previewMax;
  let target = mappedCenter === null ? fallback : mappedCenter - win.innerHeight / 2;
  if (source.scrollTop <= 0) target = 0;
  else if (source.scrollTop >= sourceMax - 1) target = previewMax;
  withScrollLock(() => win.scrollTo(0, target));
}
function syncSourceFromPreview() {
  const win = preview.contentWindow;
  const doc = preview.contentDocument;
  if (!win || !doc?.body) return;
  const previewCenter = win.scrollY + win.innerHeight / 2;
  const mappedCenter = interpolateScroll(previewCenter, 'preview', 'source');
  const ratio = scrollRatio(win.scrollY, documentHeight(doc), win.innerHeight);
  const fallback = ratio * Math.max(0, source.scrollHeight - source.clientHeight);
  const previewMax = Math.max(0, documentHeight(doc) - win.innerHeight);
  const sourceMax = Math.max(0, source.scrollHeight - source.clientHeight);
  let target = mappedCenter === null ? fallback : mappedCenter - source.clientHeight / 2;
  if (win.scrollY <= 0) target = 0;
  else if (win.scrollY >= previewMax - 1) target = sourceMax;
  withScrollLock(() => {
    source.scrollTop = target;
  });
}
function buildViewerTools() {
  const toc = document.querySelector('#toc');
  document.querySelectorAll('#article h2, #article h3').forEach((heading, index) => {
    heading.id = `section-${index + 1}`;
    const link = document.createElement('a');
    link.href = `#${heading.id}`;
    link.textContent = heading.textContent;
    link.dataset.level = heading.tagName.slice(1);
    toc.append(link);
  });
  document.querySelectorAll('#article pre').forEach((block) => {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'copy-code';
    button.textContent = 'COPY';
    button.addEventListener('click', async () => {
      await navigator.clipboard.writeText(block.querySelector('code')?.textContent || '');
      button.textContent = 'COPIED';
      setTimeout(() => button.textContent = 'COPY', 1200);
    });
    block.append(button);
  });
}
async function updatePreview() {
  const revision = ++previewRevision;
  try {
    const response = await fetch(`${location.pathname}?mode=preview`, {
      method: 'POST', headers: {'Content-Type': 'text/plain; charset=utf-8'}, body: source.value
    });
    if (!response.ok) throw new Error(await response.text());
    const markup = await response.text();
    if (revision !== previewRevision) return;
    const currentArticle = preview.contentDocument?.querySelector('article');
    if (!currentArticle) {
      preview.srcdoc = markup;
      return;
    }
    const nextDocument = new DOMParser().parseFromString(markup, 'text/html');
    const nextArticle = nextDocument.querySelector('article');
    if (!nextArticle) throw new Error('预览内容无效');
    const previewDocument = currentArticle.ownerDocument;
    currentArticle.replaceChildren(
      ...Array.from(nextArticle.childNodes, node => previewDocument.importNode(node, true))
    );
    await preview.contentWindow.renderMarkdownMermaid();
    scheduleScrollMapRebuild();
  } catch (error) {
    if (revision !== previewRevision) return;
    setStatus(`预览失败：${error.message}`, 'error');
  }
}
function schedulePreview() {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(updatePreview, 220);
}
function binaryFrame(kind, payload, updateSequence) {
  const header = updateSequence === undefined ? 1 : 5;
  const frame = new Uint8Array(header + payload.length);
  frame[0] = kind;
  if (updateSequence !== undefined) new DataView(frame.buffer).setUint32(1, updateSequence);
  frame.set(payload, header);
  return frame;
}
function sendUpdate(update) {
  if (!socket || socket.readyState !== WebSocket.OPEN) return;
  sequence = (sequence + 1) >>> 0;
  pending.add(sequence);
  socket.send(binaryFrame(1, update, sequence));
  refreshStatus();
}
function observeDocumentState() {
  documentState.on('update', (update, origin) => {
    if (origin !== REMOTE_ORIGIN) sendUpdate(update);
  });
}
function resetDocumentState() {
  documentState.destroy();
  documentState = new Y.Doc();
  sharedText = documentState.getText('source');
  pending.clear();
  latestGeneration = 0;
  savedGeneration = 0;
  observeDocumentState();
}
function restoreSelection(startPosition, endPosition) {
  const start = startPosition && Y.createAbsolutePositionFromRelativePosition(startPosition, documentState);
  const end = endPosition && Y.createAbsolutePositionFromRelativePosition(endPosition, documentState);
  const length = source.value.length;
  source.setSelectionRange(Math.min(start?.index ?? length, length), Math.min(end?.index ?? length, length));
}
function applyRemoteUpdate(update) {
  const selectable = !source.disabled;
  const start = selectable ? Y.createRelativePositionFromTypeIndex(sharedText, source.selectionStart, -1) : null;
  const end = selectable ? Y.createRelativePositionFromTypeIndex(sharedText, source.selectionEnd, 1) : null;
  Y.applyUpdate(documentState, update, REMOTE_ORIGIN);
  source.value = sharedText.toString();
  if (selectable) restoreSelection(start, end);
  schedulePreview();
}
function handleBinary(payload) {
  const frame = new Uint8Array(payload);
  if (frame[0] === 0) {
    sendUpdate(Y.encodeStateAsUpdate(documentState, frame.slice(1)));
  } else if (frame[0] === 1) {
    applyRemoteUpdate(frame.slice(1));
    synced = true;
    source.disabled = false;
    reconnectDelay = 400;
    refreshStatus();
    source.focus();
  }
}
function finishExitIfReady() {
  if (exitRequested && pending.size === 0 && savedGeneration >= latestGeneration) {
    exitRequested = false;
    socket?.close(1000, 'leave editor');
    location.reload();
  }
}
function handleControl(payload) {
  let message;
  try { message = JSON.parse(payload); } catch (_) { return; }
  if (message.type === 'room') {
    if (roomId !== null && roomId !== message.id) resetDocumentState();
    roomId = message.id;
    socket?.send(JSON.stringify({type: 'room-ready', id: roomId}));
    socket?.send(binaryFrame(0, Y.encodeStateVector(documentState)));
  } else if (message.type === 'presence') presence.textContent = `${message.count} 人在线`;
  else if (message.type === 'ack') {
    pending.delete(message.sequence);
    latestGeneration = Math.max(latestGeneration, message.generation || 0);
    refreshStatus();
  } else if (message.type === 'saved') {
    savedGeneration = Math.max(savedGeneration, message.generation || 0);
    refreshStatus();
    finishExitIfReady();
  } else if (message.type === 'error') setStatus(message.message || '协作发生错误', 'error');
}
function connect() {
  clearTimeout(reconnectTimer);
  synced = false;
  source.disabled = true;
  setStatus('正在连接…');
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  socket = new WebSocket(`${scheme}//${location.host}${location.pathname}?mode=collab`);
  socket.binaryType = 'arraybuffer';
  socket.addEventListener('message', event => {
    if (typeof event.data === 'string') handleControl(event.data);
    else handleBinary(event.data);
  });
  socket.addEventListener('close', () => {
    synced = false;
    source.disabled = true;
    pending.clear();
    if (!editing || exitRequested) return;
    setStatus('连接已断开，正在重连…', 'error');
    reconnectTimer = setTimeout(connect, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 5000);
  });
  socket.addEventListener('error', () => setStatus('协作连接失败', 'error'));
}
async function openEditor() {
  editing = true;
  reader.hidden = true;
  editor.hidden = false;
  document.body.classList.add('editing');
  await updatePreview();
  connect();
}
function syncTextareaChange() {
  if (source.disabled) return;
  const before = sharedText.toString();
  const after = source.value;
  let start = 0;
  while (start < before.length && start < after.length && before[start] === after[start]) start++;
  let beforeEnd = before.length;
  let afterEnd = after.length;
  while (beforeEnd > start && afterEnd > start && before[beforeEnd - 1] === after[afterEnd - 1]) {
    beforeEnd--;
    afterEnd--;
  }
  documentState.transact(() => {
    if (beforeEnd > start) sharedText.delete(start, beforeEnd - start);
    if (afterEnd > start) sharedText.insert(start, after.slice(start, afterEnd));
  }, LOCAL_ORIGIN);
  schedulePreview();
}
observeDocumentState();
function wrapSelection(prefix, suffix = prefix, placeholder = '文本') {
  const start = source.selectionStart;
  const end = source.selectionEnd;
  const selected = source.value.slice(start, end) || placeholder;
  source.setRangeText(`${prefix}${selected}${suffix}`, start, end, 'select');
  source.selectionStart = start + prefix.length;
  source.selectionEnd = start + prefix.length + selected.length;
  source.focus();
  syncTextareaChange();
}
function prefixLines(prefix) {
  const start = source.value.lastIndexOf('\n', Math.max(0, source.selectionStart - 1)) + 1;
  const nextBreak = source.value.indexOf('\n', source.selectionEnd);
  const end = nextBreak < 0 ? source.value.length : nextBreak;
  const text = source.value.slice(start, end).split('\n').map(line => `${prefix}${line}`).join('\n');
  source.setRangeText(text, start, end, 'select');
  source.focus();
  syncTextareaChange();
}

function escapeReviewHtml(value) {
  return String(value ?? '').replace(/[&<>"']/g, character => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
  })[character]);
}
function reviewStatusLabel(value) {
  return {open: '待处理', addressed: '待确认', resolved: '已解决'}[value] || value;
}
function showReviewError(message = '') {
  reviewError.textContent = message;
  reviewError.hidden = !message;
}
function showReviewToast(message) {
  const toast = document.querySelector('#review-toast');
  toast.textContent = message;
  toast.hidden = false;
  clearTimeout(showReviewToast.timer);
  showReviewToast.timer = setTimeout(() => { toast.hidden = true; }, 2200);
}
function validReviewIdentity(value) {
  return value.trim().length > 0;
}
function openIdentityDialog() {
  identityInput.value = reviewIdentity || '';
  identityError.textContent = '';
  if (!identityDialog.open) identityDialog.showModal();
  setTimeout(() => identityInput.focus(), 0);
}
function setReviewIdentity(value) {
  reviewIdentity = value.trim();
  localStorage.setItem(REVIEW_IDENTITY_KEY, reviewIdentity);
  identityButton.textContent = reviewIdentity;
  identityButton.title = `${reviewIdentity} · 点击切换身份`;
  identityDialog.close();
  connectReview();
  const action = pendingIdentityAction;
  pendingIdentityAction = null;
  action?.();
}
function ensureReviewIdentity() {
  const saved = localStorage.getItem(REVIEW_IDENTITY_KEY) || '';
  if (validReviewIdentity(saved)) {
    reviewIdentity = saved;
    identityButton.textContent = saved;
    identityButton.title = `${saved} · 点击切换身份`;
  } else {
    identityButton.textContent = '设置名称';
    identityButton.title = '设置审阅人名称';
  }
  connectReview();
}
function requireReviewIdentity(action) {
  if (validReviewIdentity(reviewIdentity)) {
    action();
    return;
  }
  pendingIdentityAction = action;
  openIdentityDialog();
}
async function loadReview() {
  try {
    const response = await fetch(`${location.pathname}?mode=review-data`, {cache: 'no-store'});
    if (!response.ok) throw new Error((await response.text()).trim() || '无法读取审阅数据');
    reviewDocument = await response.json();
    if (!Array.isArray(reviewDocument.comments)) reviewDocument.comments = [];
    showReviewError();
    renderReview();
  } catch (error) {
    showReviewError(error.message);
    commentList.innerHTML = '<div class="comment-empty">审阅数据暂时不可用，正文仍可正常阅读和编辑。</div>';
  }
}
async function postReviewAction(action) {
  showReviewError();
  const response = await fetch(`${location.pathname}?mode=review-action`, {
    method: 'POST',
    headers: {'Content-Type': 'application/json; charset=utf-8'},
    body: JSON.stringify(action)
  });
  if (!response.ok) throw new Error((await response.text()).trim() || '无法保存评论');
  reviewDocument = await response.json();
  renderReview();
}
function connectReview() {
  clearTimeout(reviewReconnectTimer);
  reviewSocket?.close(1000, 'identity changed');
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const socket = new WebSocket(`${scheme}//${location.host}${location.pathname}?mode=review-collab`);
  reviewSocket = socket;
  reviewUsers.textContent = '正在连接…';
  socket.addEventListener('open', () => {
    reviewReconnectDelay = 500;
    if (validReviewIdentity(reviewIdentity)) {
      socket.send(JSON.stringify({type: 'join', user: reviewIdentity}));
    }
  });
  socket.addEventListener('message', event => {
    let message;
    try { message = JSON.parse(event.data); } catch (_) { return; }
    if (message.type === 'review-updated') loadReview();
    else if (message.type === 'review-presence') {
      const users = Array.isArray(message.users) ? message.users : [];
      reviewUsers.textContent = users.length ? users.join(' · ') : '暂无在线审阅者';
    } else if (message.type === 'review-error') showReviewError(message.message || '审阅同步失败');
  });
  socket.addEventListener('close', () => {
    if (reviewSocket !== socket) return;
    reviewUsers.textContent = '连接已断开，正在重连…';
    reviewReconnectTimer = setTimeout(connectReview, reviewReconnectDelay);
    reviewReconnectDelay = Math.min(reviewReconnectDelay * 2, 5000);
  });
  socket.addEventListener('error', () => {
    if (reviewSocket === socket) reviewUsers.textContent = '审阅连接失败';
  });
}
function findUnique(haystack, needle) {
  if (!needle) return -1;
  const first = haystack.indexOf(needle);
  if (first < 0 || haystack.indexOf(needle, first + 1) >= 0) return -1;
  return first;
}
function resolveReviewScope(scope) {
  if (!scope || scope.type !== 'range') return null;
  const text = source.value;
  const quote = String(scope.quote || '');
  let start = Number(scope.start);
  let end = Number(scope.end);
  if (Number.isFinite(start) && Number.isFinite(end) && text.slice(start, end) === quote) {
    return {start, end, stale: false};
  }
  const prefix = String(scope.prefix || '');
  const suffix = String(scope.suffix || '');
  const contextual = `${prefix}${quote}${suffix}`;
  let found = findUnique(text, contextual);
  if (found >= 0) {
    start = found + prefix.length;
    return {start, end: start + quote.length, stale: false};
  }
  found = findUnique(text, quote);
  if (found >= 0) return {start: found, end: found + quote.length, stale: false};
  return {start: -1, end: -1, stale: true};
}
function markReviewRanges(resolvedScopes) {
  article.querySelectorAll('.source-run').forEach(run => {
    run.classList.remove('has-review', 'has-addressed-review');
    delete run.dataset.commentIds;
  });
  reviewDocument.comments.forEach(comment => {
    if (comment.status === 'resolved') return;
    const resolved = resolvedScopes.get(comment.id);
    if (!resolved || resolved.stale) return;
    article.querySelectorAll('.source-run').forEach(run => {
      const start = Number(run.dataset.sourceStart);
      const end = Number(run.dataset.sourceEnd);
      if (end <= resolved.start || start >= resolved.end) return;
      run.classList.add(comment.status === 'addressed' ? 'has-addressed-review' : 'has-review');
      const ids = run.dataset.commentIds ? run.dataset.commentIds.split(',') : [];
      if (!ids.includes(comment.id)) ids.push(comment.id);
      run.dataset.commentIds = ids.join(',');
    });
  });
}
function scrollToCommentSource(commentId) {
  const comment = reviewDocument.comments.find(item => item.id === commentId);
  if (!comment) return;
  const reduceMotion = matchMedia('(prefers-reduced-motion:reduce)').matches;
  if (overlayReviewLayout.matches) closeReviewPanel();
  if (comment.scope?.type === 'document') {
    article.scrollIntoView({behavior: reduceMotion ? 'auto' : 'smooth', block: 'start'});
    return;
  }
  const resolved = resolveReviewScope(comment.scope);
  if (!resolved || resolved.stale) {
    showReviewToast('原文已变化，无法定位这条评论');
    return;
  }
  const runs = Array.from(article.querySelectorAll('.source-run')).filter(run => {
    const start = Number(run.dataset.sourceStart);
    const end = Number(run.dataset.sourceEnd);
    return end > resolved.start && start < resolved.end;
  });
  if (!runs.length) {
    showReviewToast('没有找到对应的正文文本');
    return;
  }
  article.querySelectorAll('.review-target').forEach(run => run.classList.remove('review-target'));
  runs.forEach(run => run.classList.add('review-target'));
  runs[0].scrollIntoView({behavior: reduceMotion ? 'auto' : 'smooth', block: 'center'});
  setTimeout(() => runs.forEach(run => run.classList.remove('review-target')), 1800);
}
function messageMarkup(message, index, commentId) {
  const date = new Date(message.created_at);
  const shownDate = Number.isNaN(date.getTime()) ? message.created_at : date.toLocaleString();
  const edited = message.edited_at ? ' · 已编辑' : '';
  return `<section class="message" data-comment-id="${escapeReviewHtml(commentId)}" data-message-id="${escapeReviewHtml(message.id || '')}" data-message-index="${index}"><header><strong>${escapeReviewHtml(message.author)}</strong><span><time>${escapeReviewHtml(shownDate)}</time>${edited}</span></header><p>${escapeReviewHtml(message.body)}</p><div class="message-tools"><button type="button" data-message-action="edit">修改</button><button type="button" data-message-action="delete">删除</button></div></section>`;
}
function commentMarkup(comment, resolved) {
  const documentScope = comment.scope?.type === 'document';
  const quote = documentScope
    ? '全文评论'
    : (comment.scope?.display_quote || comment.scope?.quote || '选区评论');
  const stale = !documentScope && resolved?.stale;
  let buttons = documentScope
    ? '<button type="button" data-comment-action="edit-comment">编辑全文评论</button>'
    : '';
  buttons += '<button type="button" data-comment-action="reply">回复</button>';
  if (comment.status === 'open') {
    buttons += '<button class="primary" type="button" data-comment-action="resolve">标记解决</button>';
  } else if (comment.status === 'addressed') {
    buttons += '<button class="primary" type="button" data-comment-action="resolve">确认解决</button><button type="button" data-comment-action="reopen">重新打开</button>';
  } else if (comment.status === 'resolved') {
    buttons += '<button type="button" data-comment-action="reopen">重新打开</button>';
  }
  buttons += `<button type="button" data-comment-action="delete-comment">${documentScope ? '删除全文评论' : '删除整条评论'}</button>`;
  return `<article class="comment-card${selectedCommentId === comment.id ? ' active' : ''}" data-comment-id="${escapeReviewHtml(comment.id)}" title="双击定位正文"><div class="comment-meta"><span class="comment-status ${escapeReviewHtml(comment.status)}">${escapeReviewHtml(reviewStatusLabel(comment.status))}</span><span>${comment.messages.length} 条消息</span></div><p class="comment-scope${stale ? ' stale' : ''}">${stale ? '原文已变化 · ' : ''}${escapeReviewHtml(quote)}</p>${comment.messages.map((message, index) => messageMarkup(message, index, comment.id)).join('')}<div class="comment-actions">${buttons}</div></article>`;
}
function renderReview() {
  const comments = reviewDocument.comments || [];
  const counts = {open: 0, addressed: 0, resolved: 0};
  comments.forEach(comment => { if (counts[comment.status] !== undefined) counts[comment.status] += 1; });
  document.querySelectorAll('[data-review-filter]').forEach(button => {
    button.classList.toggle('active', button.dataset.reviewFilter === reviewFilter);
    button.querySelector('b').textContent = counts[button.dataset.reviewFilter] || 0;
  });
  document.querySelector('#review-count').textContent = counts.open + counts.addressed;
  document.querySelector('#review-complete').hidden = comments.length === 0 || counts.open + counts.addressed > 0;
  const resolvedScopes = new Map(comments.map(comment => [comment.id, resolveReviewScope(comment.scope)]));
  const visible = comments
    .map((comment, index) => ({comment, index, resolved: resolvedScopes.get(comment.id)}))
    .filter(item => item.comment.status === reviewFilter)
    .sort((left, right) => {
      const position = item => {
        if (item.comment.scope?.type === 'document') return -1;
        if (!item.resolved || item.resolved.stale) return Number.POSITIVE_INFINITY;
        return item.resolved.start;
      };
      return position(left) - position(right) || left.index - right.index;
    });
  commentList.innerHTML = visible.length
    ? visible.map(item => commentMarkup(item.comment, item.resolved)).join('')
    : `<div class="comment-empty">${reviewFilter === 'open' ? '暂无待处理评论。划选正文即可添加批注。' : `暂无${reviewStatusLabel(reviewFilter)}评论。`}</div>`;
  markReviewRanges(resolvedScopes);
  if (selectedCommentId) {
    requestAnimationFrame(() => commentList.querySelector(`[data-comment-id="${CSS.escape(selectedCommentId)}"]`)?.scrollIntoView({block: 'nearest'}));
  }
}
function sourceRunForBoundary(node) {
  const element = node.nodeType === Node.ELEMENT_NODE ? node : node.parentElement;
  return element?.closest?.('.source-run');
}
function sourceOffsetAtBoundary(run, node, offset) {
  const range = document.createRange();
  range.selectNodeContents(run);
  try { range.setEnd(node, offset); } catch (_) { return Number(run.dataset.sourceStart); }
  const visibleOffset = range.toString().length;
  const visibleLength = run.textContent.length;
  const sourceStart = Number(run.dataset.sourceStart);
  const sourceEnd = Number(run.dataset.sourceEnd);
  if (visibleOffset <= 0) return sourceStart;
  if (visibleOffset >= visibleLength) return sourceEnd;
  if (visibleLength === sourceEnd - sourceStart) return sourceStart + visibleOffset;
  return sourceStart + Math.round((sourceEnd - sourceStart) * visibleOffset / visibleLength);
}
function scopeFromSelection() {
  const selection = window.getSelection();
  if (!selection || selection.isCollapsed || !selection.rangeCount) return null;
  const range = selection.getRangeAt(0);
  if (!article.contains(range.commonAncestorContainer)) return null;
  const startRun = sourceRunForBoundary(range.startContainer);
  const endRun = sourceRunForBoundary(range.endContainer);
  if (!startRun || !endRun) return null;
  const start = sourceOffsetAtBoundary(startRun, range.startContainer, range.startOffset);
  const end = sourceOffsetAtBoundary(endRun, range.endContainer, range.endOffset);
  if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start) return null;
  const quote = source.value.slice(start, end);
  if (!quote.trim()) return null;
  return {
    scope: {
      type: 'range', start, end, quote,
      display_quote: selection.toString(),
      prefix: source.value.slice(Math.max(0, start - 64), start),
      suffix: source.value.slice(end, end + 64)
    },
    label: selection.toString()
  };
}
function updateSelectionComment() {
  if (editing) return;
  const selected = scopeFromSelection();
  if (!selected) {
    selectionComment.hidden = true;
    return;
  }
  pendingCommentScope = selected.scope;
  pendingSelectionLabel = selected.label;
  const range = window.getSelection().getRangeAt(0);
  const rect = range.getBoundingClientRect();
  selectionComment.style.left = `${Math.min(innerWidth - 70, Math.max(70, rect.left + rect.width / 2))}px`;
  selectionComment.style.top = `${Math.max(54, rect.top - 7)}px`;
  selectionComment.hidden = false;
}
function openReviewPanel() {
  document.body.classList.remove('review-closed');
  document.querySelector('#review-toggle').setAttribute('aria-expanded', 'true');
}
function closeReviewPanel() {
  document.body.classList.add('review-closed');
  document.querySelector('#review-toggle').setAttribute('aria-expanded', 'false');
}
function openComposer(scope, label) {
  openReviewPanel();
  pendingCommentScope = scope;
  pendingSelectionLabel = label;
  composerScope.textContent = scope.type === 'document' ? '针对全文' : `“${label.trim()}”`;
  reviewComposer.hidden = false;
  commentBody.value = '';
  commentBody.focus();
}
function closeComposer() {
  reviewComposer.hidden = true;
  commentBody.value = '';
  pendingCommentScope = null;
  pendingSelectionLabel = '';
}
function newReviewId() {
  return globalThis.crypto?.randomUUID?.() || `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}
async function submitComment() {
  if (!validReviewIdentity(reviewIdentity)) return requireReviewIdentity(submitComment);
  const body = commentBody.value.trim();
  if (!body || !pendingCommentScope) return;
  const action = {
    type: 'add-comment',
    comment: {
      id: newReviewId(), scope: pendingCommentScope, status: 'open',
      messages: [{id: newReviewId(), author: reviewIdentity, body, created_at: new Date().toISOString()}]
    }
  };
  try {
    await postReviewAction(action);
    closeComposer();
    window.getSelection()?.removeAllRanges();
    selectionComment.hidden = true;
    reviewFilter = 'open';
    renderReview();
    showReviewToast('评论已保存');
  } catch (error) { showReviewError(error.message); }
}
function openReply(card) {
  card.querySelector('.reply-box')?.remove();
  const box = document.createElement('div');
  box.className = 'reply-box';
  box.innerHTML = '<textarea rows="3" aria-label="回复内容" placeholder="Enter 提交 · ⌘ Enter 换行"></textarea><div><button class="text-button" type="button" data-reply-cancel>取消</button><button class="button" type="button" data-reply-submit>发送回复</button></div>';
  card.append(box);
  box.querySelector('textarea').focus();
}
async function handleCommentAction(button) {
  const card = button.closest('.comment-card');
  const commentId = card?.dataset.commentId;
  if (!commentId) return;
  const action = button.dataset.commentAction;
  if (action === 'reply') return requireReviewIdentity(() => openReply(card));
  if (action === 'edit-comment') {
    const message = card.querySelector('.message');
    if (message) return requireReviewIdentity(() => openMessageEditor(message));
    return;
  }
  if (action === 'delete-comment') {
    if (button.dataset.confirming === 'true') {
      try {
        await postReviewAction({type: 'delete-comment', comment_id: commentId});
        showReviewToast('整条评论已删除');
      } catch (error) { showReviewError(error.message); }
      return;
    }
    button.dataset.confirming = 'true';
    button.dataset.defaultLabel = button.textContent;
    button.textContent = `确认${button.dataset.defaultLabel}`;
    setTimeout(() => {
      if (!button.isConnected) return;
      delete button.dataset.confirming;
      button.textContent = button.dataset.defaultLabel;
      delete button.dataset.defaultLabel;
    }, 3000);
    return;
  }
  const status = action === 'resolve' ? 'resolved' : 'open';
  try {
    await postReviewAction({type: 'set-status', comment_id: commentId, status});
    showReviewToast(status === 'resolved' ? '已确认解决' : '评论已重新打开');
  } catch (error) { showReviewError(error.message); }
}
async function submitReply(card) {
  if (!validReviewIdentity(reviewIdentity)) return requireReviewIdentity(() => submitReply(card));
  const textarea = card.querySelector('.reply-box textarea');
  const body = textarea?.value.trim();
  if (!body) return;
  const comment = reviewDocument.comments.find(item => item.id === card.dataset.commentId);
  try {
    await postReviewAction({
      type: 'add-message', comment_id: card.dataset.commentId,
      message: {id: newReviewId(), author: reviewIdentity, body, created_at: new Date().toISOString()},
      status: comment?.status === 'open' ? null : 'open'
    });
    showReviewToast('回复已保存');
  } catch (error) { showReviewError(error.message); }
}
function messageReference(messageElement) {
  return {
    comment_id: messageElement.dataset.commentId,
    message_id: messageElement.dataset.messageId || null,
    message_index: Number(messageElement.dataset.messageIndex)
  };
}
function openMessageEditor(messageElement) {
  messageElement.querySelector('.message-editor')?.remove();
  const editor = document.createElement('div');
  editor.className = 'message-editor';
  editor.innerHTML = '<textarea rows="3" aria-label="修改消息" placeholder="Enter 提交 · ⌘ Enter 换行"></textarea><div><button class="text-button" type="button" data-message-edit-cancel>取消</button><button class="button" type="button" data-message-edit-submit>保存修改</button></div>';
  editor.querySelector('textarea').value = messageElement.querySelector('p').textContent;
  messageElement.append(editor);
  editor.querySelector('textarea').focus();
}
async function submitMessageEdit(messageElement) {
  const body = messageElement.querySelector('.message-editor textarea')?.value.trim();
  if (!body) return;
  try {
    await postReviewAction({
      type: 'edit-message', ...messageReference(messageElement), body,
      edited_at: new Date().toISOString()
    });
    showReviewToast('消息已修改');
  } catch (error) { showReviewError(error.message); }
}
async function deleteMessage(messageElement) {
  try {
    await postReviewAction({type: 'delete-message', ...messageReference(messageElement)});
    showReviewToast('消息已删除');
  } catch (error) { showReviewError(error.message); }
}
document.querySelector('#edit-button').addEventListener('click', openEditor);
document.querySelector('#review-toggle').addEventListener('click', () => {
  const closing = !document.body.classList.contains('review-closed');
  document.body.classList.toggle('review-closed', closing);
  document.querySelector('#review-toggle').setAttribute('aria-expanded', String(!closing));
});
document.querySelector('#review-close').addEventListener('click', () => {
  closeReviewPanel();
});
identityButton.addEventListener('click', () => {
  pendingIdentityAction = null;
  openIdentityDialog();
});
identityDialog.addEventListener('cancel', () => {
  pendingIdentityAction = null;
});
identityDialog.querySelector('form').addEventListener('submit', event => {
  event.preventDefault();
  const value = identityInput.value.trim();
  if (!validReviewIdentity(value)) {
    identityError.textContent = '请输入审阅人名称';
    identityInput.focus();
    return;
  }
  setReviewIdentity(value);
});
document.querySelector('#document-comment').addEventListener('click', () => {
  requireReviewIdentity(() => openComposer({type: 'document'}, '全文评论'));
});
function activateSelectionComment() {
  const scope = pendingCommentScope;
  const label = pendingSelectionLabel;
  if (scope) requireReviewIdentity(() => openComposer(scope, label));
  selectionComment.hidden = true;
}
selectionComment.addEventListener('touchend', event => {
  event.preventDefault();
  activateSelectionComment();
}, {passive: false});
selectionComment.addEventListener('click', activateSelectionComment);
document.querySelector('#composer-cancel').addEventListener('click', closeComposer);
document.querySelector('#composer-submit').addEventListener('click', submitComment);
commentBody.addEventListener('keydown', event => {
  if (event.key !== 'Enter' || event.isComposing || event.keyCode === 229) return;
  event.preventDefault();
  if (event.metaKey) {
    commentBody.setRangeText('\n', commentBody.selectionStart, commentBody.selectionEnd, 'end');
  } else submitComment();
});
document.querySelector('.review-filters').addEventListener('click', event => {
  const filter = event.target.closest('[data-review-filter]')?.dataset.reviewFilter;
  if (!filter) return;
  reviewFilter = filter;
  selectedCommentId = null;
  renderReview();
});
commentList.addEventListener('click', event => {
  const actionButton = event.target.closest('[data-comment-action]');
  if (actionButton) return handleCommentAction(actionButton);
  const messageElement = event.target.closest('.message');
  const messageAction = event.target.closest('[data-message-action]');
  if (messageAction?.dataset.messageAction === 'edit') {
    return requireReviewIdentity(() => openMessageEditor(messageElement));
  }
  if (messageAction?.dataset.messageAction === 'delete') {
    if (messageAction.dataset.confirming === 'true') return deleteMessage(messageElement);
    messageAction.dataset.confirming = 'true';
    messageAction.textContent = '确认删除';
    setTimeout(() => {
      if (!messageAction.isConnected) return;
      delete messageAction.dataset.confirming;
      messageAction.textContent = '删除';
    }, 3000);
    return;
  }
  if (event.target.closest('[data-message-edit-cancel]')) return messageElement.querySelector('.message-editor')?.remove();
  if (event.target.closest('[data-message-edit-submit]')) return submitMessageEdit(messageElement);
  const card = event.target.closest('.comment-card');
  if (event.target.closest('[data-reply-cancel]')) return card.querySelector('.reply-box')?.remove();
  if (event.target.closest('[data-reply-submit]')) return submitReply(card);
});
commentList.addEventListener('keydown', event => {
  if (event.key !== 'Enter' || event.isComposing || event.keyCode === 229) return;
  const isReply = event.target.matches('.reply-box textarea');
  const isEdit = event.target.matches('.message-editor textarea');
  if (!isReply && !isEdit) return;
  event.preventDefault();
  if (event.metaKey) {
    event.target.setRangeText('\n', event.target.selectionStart, event.target.selectionEnd, 'end');
    return;
  }
  if (isReply) {
    submitReply(event.target.closest('.comment-card'));
  } else if (isEdit) {
    submitMessageEdit(event.target.closest('.message'));
  }
});
commentList.addEventListener('dblclick', event => {
  if (event.target.closest('button,textarea')) return;
  const commentId = event.target.closest('.comment-card')?.dataset.commentId;
  if (commentId) scrollToCommentSource(commentId);
});
article.addEventListener('mouseup', () => setTimeout(updateSelectionComment, 0));
article.addEventListener('keyup', () => setTimeout(updateSelectionComment, 0));
article.addEventListener('touchend', () => setTimeout(updateSelectionComment, 80), {passive: true});
document.addEventListener('selectionchange', () => {
  clearTimeout(updateSelectionComment.timer);
  updateSelectionComment.timer = setTimeout(updateSelectionComment, 80);
});
article.addEventListener('click', event => {
  const selection = window.getSelection();
  if (touchReviewDevice.matches
      && !document.body.classList.contains('review-closed')
      && selection?.isCollapsed) {
    closeReviewPanel();
    return;
  }
  const ids = event.target.closest('.source-run[data-comment-ids]')?.dataset.commentIds?.split(',');
  if (!ids?.length || !selection?.isCollapsed) return;
  const comment = reviewDocument.comments.find(item => item.id === ids[0]);
  if (!comment) return;
  selectedCommentId = comment.id;
  reviewFilter = comment.status;
  openReviewPanel();
  renderReview();
});
document.addEventListener('pointerdown', event => {
  if (!selectionComment.contains(event.target) && !article.contains(event.target)) {
    selectionComment.hidden = true;
  }
});
document.querySelector('#cancel-button').addEventListener('click', () => {
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    setStatus('连接恢复后才能安全退出', 'error');
    return;
  }
  exitRequested = true;
  source.disabled = true;
  setStatus('正在保存并退出…', 'connected');
  socket.send(JSON.stringify({type: 'save'}));
});
document.querySelector('#save-button').addEventListener('click', () => {
  if (socket?.readyState === WebSocket.OPEN) {
    setStatus('正在保存…', 'connected');
    socket.send(JSON.stringify({type: 'save'}));
  }
});
document.querySelector('.format-tools').addEventListener('click', event => {
  const action = event.target.closest('button')?.dataset.format;
  if (!action || source.disabled) return;
  if (action === 'heading') prefixLines('## ');
  if (action === 'bold') wrapSelection('**');
  if (action === 'italic') wrapSelection('_');
  if (action === 'link') wrapSelection('[', '](https://)', '链接文字');
  if (action === 'quote') prefixLines('> ');
  if (action === 'code') wrapSelection('`');
  if (action === 'list') prefixLines('- ');
  if (action === 'task') prefixLines('- [ ] ');
});
source.addEventListener('input', syncTextareaChange);
source.addEventListener('scroll', syncPreviewFromSource, {passive: true});
preview.addEventListener('load', () => {
  const win = preview.contentWindow;
  const doc = preview.contentDocument;
  win?.addEventListener('scroll', syncSourceFromPreview, {passive: true});
  doc?.addEventListener('load', event => {
    if (event.target instanceof win.HTMLImageElement) scheduleScrollMapRebuild();
  }, true);
  previewResizeObserver?.disconnect();
  const article = doc?.querySelector('article');
  if (article && win?.ResizeObserver) {
    previewResizeObserver = new win.ResizeObserver(() => scheduleScrollMapRebuild());
    previewResizeObserver.observe(article);
  }
  scheduleScrollMapRebuild();
});
new ResizeObserver(() => scheduleScrollMapRebuild()).observe(source);
source.addEventListener('keydown', event => {
  if (event.key === 'Tab') {
    event.preventDefault();
    source.setRangeText('  ', source.selectionStart, source.selectionEnd, 'end');
    syncTextareaChange();
  }
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 's') {
    event.preventDefault();
    document.querySelector('#save-button').click();
  }
});
window.addEventListener('beforeunload', event => {
  if (editing && (pending.size > 0 || !synced)) {
    event.preventDefault();
    event.returnValue = '';
  }
});
buildViewerTools();
loadReview();
ensureReviewIdentity();
"#;

fn send_text(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
    head_only: bool,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if !head_only {
        stream.write_all(body.as_bytes())?;
    }
    Ok(())
}

fn safe_relative_path(path: &str) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    for component in Path::new(path.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(result)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            decoded.push(u8::from_str_radix(text, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn query_parameter(query: &str, expected: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = percent_decode(&name.replace('+', " "))?;
        if name != expected {
            return None;
        }
        percent_decode(&value.replace('+', " "))
    })
}

fn mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "drawio" => "application/vnd.jgraph.mxfile",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_http_request(
        address: std::net::SocketAddr,
        method: &str,
        target: &str,
        body: &str,
    ) -> String {
        let mut client = TcpStream::connect(address).unwrap();
        write!(
            client,
            "{method} {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn parses_default_and_custom_ports() {
        assert_eq!(
            parse_args(Vec::<String>::new().into_iter()).unwrap(),
            Some(PortConfig {
                port: 8080,
                fallback_to_random: true,
                pid_file: None,
                web: false,
                cache_dir: None,
                serve_dir: PathBuf::from("."),
            })
        );
        assert_eq!(
            parse_args(vec!["-p".into(), "3000".into()].into_iter()).unwrap(),
            Some(PortConfig {
                port: 3000,
                fallback_to_random: false,
                pid_file: None,
                web: false,
                cache_dir: None,
                serve_dir: PathBuf::from("."),
            })
        );
        assert_eq!(
            parse_args(
                vec![
                    "-pid".into(),
                    "/tmp/http.pid".into(),
                    "-p".into(),
                    "9000".into()
                ]
                .into_iter()
            )
            .unwrap(),
            Some(PortConfig {
                port: 9000,
                fallback_to_random: false,
                pid_file: Some(PathBuf::from("/tmp/http.pid")),
                web: false,
                cache_dir: None,
                serve_dir: PathBuf::from("."),
            })
        );
        assert_eq!(
            parse_args(vec!["--web".into()].into_iter()).unwrap(),
            Some(PortConfig {
                port: 8080,
                fallback_to_random: true,
                pid_file: None,
                web: true,
                cache_dir: None,
                serve_dir: PathBuf::from("."),
            })
        );
        assert!(parse_args(vec!["-pid".into()].into_iter()).is_err());
    }

    #[test]
    fn falls_back_to_a_random_port_when_default_is_in_use() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let listener = bind_listener(&PortConfig {
            port: occupied_port,
            fallback_to_random: true,
            pid_file: None,
            web: false,
            cache_dir: None,
            serve_dir: PathBuf::from("."),
        })
        .unwrap();

        assert_ne!(listener.local_addr().unwrap().port(), occupied_port);
    }

    #[test]
    fn explicit_port_does_not_fall_back_when_it_is_in_use() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let error = bind_listener(&PortConfig {
            port: occupied_port,
            fallback_to_random: false,
            pid_file: None,
            web: false,
            cache_dir: None,
            serve_dir: PathBuf::from("."),
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn writes_the_current_process_id_to_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.pid");

        write_pid_file(&path).unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            format!("{}\n", std::process::id())
        );
    }

    #[test]
    fn parses_cache_and_serving_directories() {
        for (cache_flag, dir_flag) in [("-cache", "-dir"), ("--cache", "--dir")] {
            let config = parse_args(
                vec![
                    cache_flag.into(),
                    "image cache".into(),
                    dir_flag.into(),
                    "my photos".into(),
                ]
                .into_iter(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(config.cache_dir, Some(PathBuf::from("image cache")));
            assert_eq!(config.serve_dir, PathBuf::from("my photos"));
            assert!(parse_args(vec![cache_flag.into()].into_iter()).is_err());
            assert!(parse_args(vec![dir_flag.into()].into_iter()).is_err());
        }
    }

    fn image_request(
        root: &Path,
        cache: Option<&ImageCache>,
        method: &str,
        mode: &str,
        etag: &str,
    ) -> (String, Vec<u8>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let root = root.to_owned();
        let cache = cache.cloned();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(
                stream,
                &root,
                &CollaborationHub::default(),
                &ReviewHub::default(),
                false,
                cache.as_ref(),
                &StateStore::new(None).unwrap(),
            )
            .unwrap();
        });
        let mut stream = TcpStream::connect(address).unwrap();
        write!(stream, "{method} /photo.png?mode={mode} HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: {etag}\r\n\r\n").unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        server.join().unwrap();
        let split = response
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        (
            String::from_utf8(response[..split].to_vec()).unwrap(),
            response[split..].to_vec(),
        )
    }

    #[test]
    fn image_cache_revalidates_overwritten_and_recreated_sources() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("photo.png");
        let cache = ImageCache::new(&directory.path().join("cache")).unwrap();
        let original_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        let write_image = |color| {
            let mut source = Cursor::new(Vec::new());
            image::RgbImage::from_pixel(800, 200, image::Rgb(color))
                .write_to(&mut source, image::ImageFormat::Png)
                .unwrap();
            // Keep size and mtime identical while changing the image content.
            let mut source = source.into_inner();
            source.resize(8192, 0);
            fs::write(&path, source).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(original_time))
                .unwrap();
        };
        for mode in ["thumb", "gallery-thumb", "gallery-preview", "asset"] {
            write_image([240, 20, 30]);
            let (headers, first) = image_request(directory.path(), Some(&cache), "GET", mode, "");
            assert!(headers.starts_with("HTTP/1.1 200"));
            assert!(headers.contains("Cache-Control: no-cache\r\n"));
            let etag = headers
                .lines()
                .find_map(|line| line.strip_prefix("ETag: "))
                .unwrap();
            let (headers, body) = image_request(directory.path(), Some(&cache), "GET", mode, etag);
            assert!(headers.starts_with("HTTP/1.1 304"));
            assert!(body.is_empty());

            let (headers, body) = image_request(directory.path(), Some(&cache), "HEAD", mode, "");
            assert!(headers.starts_with("HTTP/1.1 200"));
            assert!(headers.contains(&format!("Content-Length: {}\r\n", first.len())));
            assert!(body.is_empty());

            write_image([20, 30, 240]);
            let (headers, changed) =
                image_request(directory.path(), Some(&cache), "GET", mode, etag);
            assert!(headers.starts_with("HTTP/1.1 200"));
            assert_ne!(first, changed);

            fs::remove_file(&path).unwrap();
            let (headers, _) = image_request(directory.path(), Some(&cache), "GET", mode, etag);
            assert!(headers.starts_with("HTTP/1.1 404"));
            write_image([20, 240, 30]);
            let (headers, recreated) =
                image_request(directory.path(), Some(&cache), "GET", mode, etag);
            assert!(headers.starts_with("HTTP/1.1 200"));
            assert_ne!(first, recreated);
            assert_ne!(changed, recreated);
            image::load_from_memory(&recreated).unwrap();
        }
    }

    #[test]
    fn image_requests_without_cache_always_return_current_content() {
        let directory = tempfile::tempdir().unwrap();
        image::RgbImage::from_pixel(32, 32, image::Rgb([20, 40, 60]))
            .save(directory.path().join("photo.png"))
            .unwrap();
        for mode in ["thumb", "gallery-thumb", "gallery-preview", "asset"] {
            let (headers, body) = image_request(directory.path(), None, "GET", mode, "*");
            assert!(headers.starts_with("HTTP/1.1 200"));
            assert!(headers.contains("Cache-Control: no-store\r\n"));
            image::load_from_memory(&body).unwrap();
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn versioned_image_requests_use_immutable_browser_caching() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("photo.png");
        image::RgbImage::from_pixel(32, 32, image::Rgb([20, 40, 60]))
            .save(&path)
            .unwrap();
        let cache = ImageCache::new(&directory.path().join("cache")).unwrap();
        let version = file_version(&fs::metadata(&path).unwrap());

        for mode in ["thumb", "gallery-thumb", "gallery-preview", "asset"] {
            let mode = format!("{mode}&v={version}");
            let (headers, _) = image_request(directory.path(), Some(&cache), "GET", &mode, "");
            assert!(headers.starts_with("HTTP/1.1 200"));
            assert!(headers.contains("Cache-Control: private, max-age=31536000, immutable\r\n"));
        }
    }

    #[test]
    fn web_mode_serves_index_and_files_without_special_rendering() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("index.html"), "<h1>home</h1>").unwrap();
        fs::write(directory.path().join("notes.md"), "# Notes\n").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let collaboration = CollaborationHub::default();
            let reviews = ReviewHub::default();
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                handle_connection(
                    stream,
                    &root,
                    &collaboration,
                    &reviews,
                    true,
                    None,
                    &StateStore::new(None).unwrap(),
                )
                .unwrap();
            }
        });

        let request = |target: &str| {
            let mut client = TcpStream::connect(address).unwrap();
            write!(
                client,
                "GET {target} HTTP/1.1\r\nHost: {address}\r\nAccept: text/html\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            response
        };

        let index = request("/");
        assert!(index.starts_with("HTTP/1.1 200 OK"));
        assert!(index.contains("Content-Type: text/html; charset=utf-8"));
        assert!(index.ends_with("<h1>home</h1>"));

        let markdown = request("/notes.md?mode=preview");
        assert!(markdown.starts_with("HTTP/1.1 200 OK"));
        assert!(markdown.contains("Content-Type: text/markdown; charset=utf-8"));
        assert!(markdown.ends_with("# Notes\n"));
        assert!(!markdown.contains("<!doctype html>"));

        let favicon = request("/favicon.svg");
        assert!(favicon.starts_with("HTTP/1.1 404 Not Found"));
        assert!(favicon.ends_with("Not Found\n"));
        server.join().unwrap();
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(safe_relative_path("../secret").is_none());
        assert!(safe_relative_path("assets/app.js").is_some());
    }

    #[test]
    fn decodes_url_paths() {
        assert_eq!(
            percent_decode("/hello%20world.txt").as_deref(),
            Some("/hello world.txt")
        );
        assert!(percent_decode("/%zz").is_none());
    }

    #[test]
    fn encodes_directory_links() {
        assert_eq!(
            url_for_path(Path::new("文档/my notes"), true),
            "/%E6%96%87%E6%A1%A3/my%20notes/"
        );
        assert_eq!(
            url_for_path(Path::new("文档/read me.md"), false),
            "/%E6%96%87%E6%A1%A3/read%20me.md"
        );
    }

    #[test]
    fn renders_markdown_as_a_styled_html_document() {
        let page = render_markdown_page(
            "# Hello\n\n- [x] done\n\n| A | B |\n|---|---|\n| 1 | 2 |",
            "README.md",
        );

        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("<body class=\"review-closed\">"));
        assert!(page.contains("id=\"review-toggle\" type=\"button\" aria-expanded=\"false\""));
        assert!(page.contains("<h1>"));
        assert!(page.contains(">Hello</span></h1>"));
        assert!(page.contains("<table>"));
        assert!(page.contains("type=\"checkbox\""));
        assert!(page.contains("README.md"));
        assert!(page.contains("id=\"edit-button\""));
        assert!(page.contains("id=\"review-panel\""));
        assert!(page.contains("id=\"identity-dialog\""));
        assert!(page.contains("?mode=review-collab"));
        assert!(page.contains("?mode=review-action"));
        assert!(page.contains("commentList.addEventListener('dblclick'"));
        assert!(page.contains("data-comment-action=\"edit-comment\">编辑全文评论"));
        assert!(page.contains("documentScope ? '删除全文评论' : '删除整条评论'"));
        assert!(page.contains("if (action === 'edit-comment')"));
        assert!(page.contains("function scrollToCommentSource(commentId)"));
        assert!(page.contains("article.addEventListener('touchend'"));
        assert!(page.contains("document.addEventListener('selectionchange'"));
        assert!(page.contains("selectionComment.addEventListener('touchend'"));
        assert!(page.contains("function activateSelectionComment()"));
        assert!(page.contains("touchReviewDevice.matches"));
        assert!(page.contains("function closeReviewPanel()"));
        assert!(page.contains(".selection-comment { top:auto !important; bottom:max(1rem"));
        assert!(page.contains("return item.resolved.start;"));
        assert!(page.contains("function requireReviewIdentity(action)"));
        assert!(page.contains("id=\"source\""));
        assert!(page.contains("[hidden] { display:none !important; }"));
        assert!(page.contains(".editor-shell { display:flex; flex-direction:column;"));
        assert!(page.contains("#toc a { display:block; width:max-content; min-width:100%;"));
        assert!(
            page.find("class=\"pane preview-pane\"").unwrap()
                < page.find("class=\"pane source-pane\"").unwrap()
        );
        assert!(page.contains(".preview-pane { border-right:1px solid var(--line); }"));
        assert!(page.contains("source.addEventListener('scroll', syncPreviewFromSource"));
        assert!(page.contains("addEventListener('scroll', syncSourceFromPreview"));
        assert!(page.contains(YJS_PATH));
        assert!(page.contains("id=\"connection-dot\""));
        assert!(page.contains("id=\"presence\""));
        assert!(page.contains("退出编辑"));
        assert!(page.contains("立即保存"));
        assert!(page.contains("new WebSocket("));
        assert!(page.contains("async function openEditor()"));
        assert!(page.contains("await updatePreview();\n  connect();"));
        assert!(page.contains("Y.encodeStateVector(documentState)"));
        assert!(page.contains("source.addEventListener('input', syncTextareaChange)"));
        assert!(page.contains("new DOMParser().parseFromString(markup, 'text/html')"));
        assert!(page.contains("currentArticle.replaceChildren("));
        assert!(page.contains("if (revision !== previewRevision) return;"));
        assert!(!page.contains("preview.srcdoc = await response.text()"));
        assert!(!page.contains("<dialog id=\"discard-dialog\""));
        assert!(page.contains("href=\"/favicon.svg\""));
        assert!(YJS_JS.len() > 80_000);
    }

    #[test]
    fn generates_a_site_icon_from_the_root_directory_name() {
        let icon = render_site_icon(Path::new("/srv/http-file-server"));
        let escaped = render_site_icon(Path::new("/srv/<project>"));

        assert!(icon.starts_with("<svg"));
        assert!(icon.contains("viewBox=\"0 0 64 64\""));
        assert!(icon.contains(">H</text>"));
        assert!(escaped.contains(">&lt;</text>"));
        assert!(!escaped.contains("><</text>"));
    }

    #[test]
    fn renders_a_styled_and_safe_not_found_page() {
        let page = render_not_found_page("/missing/<script>.txt");

        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("HTTP 404"));
        assert!(page.contains("这个文件<br>不在这里"));
        assert!(page.contains("/missing/&lt;script&gt;.txt"));
        assert!(!page.contains("<script>.txt"));
        assert!(page.contains("href=\"/\""));
        assert!(page.contains("overflow-wrap:anywhere; white-space:pre-wrap"));
        assert!(!page.contains("text-overflow:ellipsis"));
        assert!(page.contains("@media (prefers-reduced-motion:reduce)"));
    }

    #[test]
    fn escapes_embedded_html_in_markdown() {
        let page = render_markdown_page("<script>alert('x')</script>", "unsafe.md");

        assert!(!page.contains("<script>alert('x')</script>"));
        assert!(page.contains("&lt;script&gt;"));
    }

    #[test]
    fn adds_utf16_source_anchors_to_markdown_blocks() {
        let article = render_markdown_article("你好\n\n## title");

        assert!(article.contains("data-source-offset=\"0\""));
        assert!(article.contains("data-source-offset=\"4\""));
        assert!(article.contains("<h2>"));
        assert!(article.contains(">title</span></h2>"));
        assert!(article.contains("class=\"source-run\" data-source-start=\"0\""));
    }

    #[test]
    fn maps_rendered_markdown_text_to_utf16_source_ranges() {
        let article = render_markdown_article("# 😀你好\n\n**bold** and [link](target)");

        assert!(article.contains("data-source-start=\"2\" data-source-end=\"6\">😀你好"));
        assert!(article.contains("data-source-start=\"10\" data-source-end=\"14\">bold"));
        assert!(article.contains("data-source-start=\"22\" data-source-end=\"26\">link"));
    }

    #[test]
    fn reads_mode_from_query_string() {
        assert_eq!(
            query_parameter("download=1&mode=raw", "mode").as_deref(),
            Some("raw")
        );
        assert_eq!(
            query_parameter("mode=pre%76iew", "mode").as_deref(),
            Some("preview")
        );
        assert_eq!(query_parameter("model=raw", "mode"), None);
    }

    #[test]
    fn validates_websocket_upgrade_headers() {
        let headers = RequestHeaders {
            connection: "keep-alive, upgrade".into(),
            upgrade: "websocket".into(),
            websocket_key: "dGhlIHNhbXBsZSBub25jZQ==".into(),
            websocket_version: "13".into(),
            ..RequestHeaders::default()
        };
        assert!(is_websocket_upgrade(&headers));

        let invalid = RequestHeaders {
            websocket_key: "not-a-valid-key".into(),
            ..headers
        };
        assert!(!is_websocket_upgrade(&invalid));
    }

    #[test]
    fn websocket_collaboration_syncs_and_auto_saves() {
        use tempfile::tempdir;
        use tungstenite::{connect, Message};
        use yrs::updates::decoder::Decode;
        use yrs::updates::encoder::Encode;
        use yrs::{Doc, GetString, ReadTxn, Text, Transact, Update};

        let directory = tempdir().unwrap();
        let path = directory.path().join("notes.md");
        fs::write(&path, "hello").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let hub = CollaborationHub::default();
        let reviews = ReviewHub::default();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(
                stream,
                &root,
                &hub,
                &reviews,
                false,
                None,
                &StateStore::new(None).unwrap(),
            )
            .unwrap();
        });

        let (mut socket, response) =
            connect(format!("ws://{address}/notes.md?mode=collab")).unwrap();
        assert_eq!(response.status(), 101);
        let client = Doc::with_client_id(42);
        let text = client.get_or_insert_text("source");
        let vector = client.transact().state_vector().encode_v1();
        let mut request = vec![0];
        request.extend_from_slice(&vector);
        let room_id = loop {
            if let Message::Text(message) = socket.read().unwrap() {
                let control: serde_json::Value = serde_json::from_str(&message).unwrap();
                if control["type"] == "room" {
                    break control["id"].as_str().unwrap().to_owned();
                }
            }
        };
        socket
            .send(Message::Text(
                serde_json::json!({"type": "room-ready", "id": room_id})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        socket.send(Message::Binary(request.into())).unwrap();

        loop {
            if let Message::Binary(message) = socket.read().unwrap() {
                if message.first() == Some(&1) {
                    client
                        .transact_mut()
                        .apply_update(Update::decode_v1(&message[1..]).unwrap())
                        .unwrap();
                    break;
                }
            }
        }
        assert_eq!(text.get_string(&client.transact()), "hello");

        let before = client.transact().state_vector();
        text.insert(&mut client.transact_mut(), 5, " world");
        let update = client.transact().encode_state_as_update_v1(&before);
        let mut change = vec![1];
        change.extend_from_slice(&7_u32.to_be_bytes());
        change.extend_from_slice(&update);
        socket.send(Message::Binary(change.into())).unwrap();

        let mut acknowledged = false;
        let mut saved = false;
        while !acknowledged || !saved {
            if let Message::Text(message) = socket.read().unwrap() {
                acknowledged |=
                    message.contains("\"type\":\"ack\"") && message.contains("\"sequence\":7");
                saved |= message.contains("\"type\":\"saved\"");
            }
        }
        socket.close(None).unwrap();
        server.join().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "hello world");
    }

    #[test]
    fn conservatively_detects_utf8_text() {
        assert!(looks_like_utf8_text(b"[server]\nport = 8080\n"));
        assert!(looks_like_utf8_text("标题: 配置\n".as_bytes()));
        assert!(!looks_like_utf8_text(b"text\0binary"));
        assert!(!looks_like_utf8_text(&[0xff, 0xfe, 0x00, 0x41]));
        assert!(has_binary_magic(b"%PDF-1.7 printable header"));
        assert!(has_binary_magic(b"PK\x03\x04archive"));
    }

    #[test]
    fn recognizes_text_names_and_binary_extensions() {
        assert_eq!(text_kind(Path::new("Dockerfile")), "DOCKERFILE");
        assert_eq!(text_kind(Path::new(".bashrc")), "SHELL");
        assert_eq!(text_kind(Path::new("app.yaml")), "YAML");
        assert_eq!(text_kind(Path::new("service.conf")), "CONFIG");
        assert!(has_binary_extension(Path::new("archive.zip")));
        assert!(has_binary_extension(Path::new("unknown.bin")));
        assert!(!has_binary_extension(Path::new("Cargo.toml")));
    }

    #[test]
    fn renders_a_read_only_text_viewer() {
        let page = render_text_page(
            "name = \"http\"\nport = 8080",
            "config.toml",
            "TOML",
            Path::new("config.toml"),
        );

        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("class=\"line\">name = &quot;http&quot;"));
        assert!(page.contains("2 行"));
        assert!(page.contains("?mode=raw"));
        assert!(!page.contains("id=\"edit-button\""));
        assert!(page.contains("font:500 .84rem/1.3"));
        assert!(page.contains("body.wrap .line { position:relative; padding-left:5.4rem;"));
        assert!(page.contains("body.wrap .line::before { position:absolute;"));
        assert!(page.contains("</span><span class=\"line\">"));
        assert!(!page.contains("</span>\n<span class=\"line\">"));
    }

    #[test]
    fn deletes_gallery_images_from_disk_and_directory_listing() {
        let directory = tempfile::tempdir().unwrap();
        image::RgbImage::new(2, 2)
            .save(directory.path().join("my photo.png"))
            .unwrap();
        fs::write(directory.path().join("icon.svg"), "<svg></svg>").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let collaboration = CollaborationHub::default();
            let reviews = ReviewHub::default();
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                handle_connection(
                    stream,
                    &root,
                    &collaboration,
                    &reviews,
                    false,
                    None,
                    &StateStore::new(None).unwrap(),
                )
                .unwrap();
            }
        });
        let request = |method: &str, target: &str| {
            let mut client = TcpStream::connect(address).unwrap();
            write!(
                client,
                "{method} {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            response
        };

        assert!(request("DELETE", "/my%20photo.png").starts_with("HTTP/1.1 204 No Content"));
        assert!(!directory.path().join("my photo.png").exists());
        assert!(request("DELETE", "/icon.svg").starts_with("HTTP/1.1 204 No Content"));
        assert!(!directory.path().join("icon.svg").exists());
        let listing = request("GET", "/?view=gallery");
        assert!(listing.contains("0 个目录 · 0 个文件"));
        assert!(listing.contains("这个目录是空的"));
        server.join().unwrap();
    }

    #[test]
    fn marks_gallery_images_and_deletes_all_marked_images_in_the_directory() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("cat.svg"), "<svg>cat</svg>").unwrap();
        fs::write(directory.path().join("dog.svg"), "<svg>dog</svg>").unwrap();
        fs::write(directory.path().join("notes.txt"), "keep").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let marked_path = root.join("cat.svg");
        let state = StateStore::new(None).unwrap();
        let server_state = state.clone();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                handle_connection(
                    stream,
                    &root,
                    &CollaborationHub::default(),
                    &ReviewHub::default(),
                    false,
                    None,
                    &server_state,
                )
                .unwrap();
            }
        });
        let request = |method: &str, target: &str| {
            let mut client = TcpStream::connect(address).unwrap();
            write!(
                client,
                "{method} {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            response
        };

        assert!(request("PUT", "/cat.svg?mode=deletion-mark").starts_with("HTTP/1.1 204"));
        let listing = request("GET", "/");
        assert!(listing.contains("data-deletion-marked=\"true\""));
        assert!(listing.contains("class=\"deletion-mark\" title=\"待删除\">待删除</span>"));
        let response = request("POST", "/?mode=delete-marked");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with(r#"{"deleted":1,"errors":[]}"#));
        server.join().unwrap();

        assert!(!directory.path().join("cat.svg").exists());
        assert!(directory.path().join("dog.svg").exists());
        assert!(directory.path().join("notes.txt").exists());
        assert!(!state.is_deletion_marked(&marked_path).unwrap());
    }

    #[test]
    fn clears_all_deletion_marks_in_the_current_directory() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("cat.svg"), "<svg>cat</svg>").unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        fs::write(directory.path().join("nested/dog.svg"), "<svg>dog</svg>").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let cat = root.join("cat.svg");
        let dog = root.join("nested/dog.svg");
        let state = StateStore::new(None).unwrap();
        state.set_deletion_mark(&cat, true).unwrap();
        state.set_deletion_mark(&dog, true).unwrap();

        let result = clear_directory_deletion_marks(&root, &state).unwrap();

        assert_eq!(result.cleared, 1);
        assert!(result.errors.is_empty());
        assert!(!state.is_deletion_marked(&cat).unwrap());
        assert!(state.is_deletion_marked(&dog).unwrap());
        assert!(cat.exists());
        assert!(dog.exists());
    }

    #[test]
    fn organises_images_and_keeps_favourites_at_their_new_paths() {
        for disk in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(directory.path()).unwrap();
            let cache = tempfile::tempdir().unwrap();
            let state = StateStore::new(disk.then_some(cache.path())).unwrap();
            fs::write(root.join("liked 猫.svg"), "<svg>liked</svg>").unwrap();
            fs::write(root.join("other.svg"), "<svg>other</svg>").unwrap();
            fs::write(root.join("notes.txt"), "notes").unwrap();
            fs::create_dir(root.join("nested")).unwrap();
            fs::write(root.join("nested/photo.svg"), "<svg>nested</svg>").unwrap();
            state
                .set_favourite(&root.join("liked 猫.svg"), true)
                .unwrap();
            state
                .set_deletion_mark(&root.join("liked 猫.svg"), true)
                .unwrap();
            state
                .set_deletion_mark(&root.join("other.svg"), true)
                .unwrap();

            let result = move_favourite_images(&root, &state).unwrap();
            assert_eq!(result.moved, 1);
            assert!(result.errors.is_empty());
            assert_eq!(
                fs::read_to_string(root.join("favourites/liked 猫.svg")).unwrap(),
                "<svg>liked</svg>"
            );
            assert!(state
                .is_favourite(&root.join("favourites/liked 猫.svg"))
                .unwrap());
            assert!(!state.is_favourite(&root.join("liked 猫.svg")).unwrap());
            assert!(state
                .is_deletion_marked(&root.join("favourites/liked 猫.svg"))
                .unwrap());
            assert!(!state
                .is_deletion_marked(&root.join("liked 猫.svg"))
                .unwrap());

            assert_eq!(
                fs::read_to_string(root.join("other.svg")).unwrap(),
                "<svg>other</svg>"
            );
            assert!(state.is_deletion_marked(&root.join("other.svg")).unwrap());
            assert_eq!(fs::read_to_string(root.join("notes.txt")).unwrap(), "notes");
            assert_eq!(
                fs::read_to_string(root.join("nested/photo.svg")).unwrap(),
                "<svg>nested</svg>"
            );
            if disk {
                let reopened = StateStore::new(Some(cache.path())).unwrap();
                assert!(reopened
                    .is_favourite(&root.join("favourites/liked 猫.svg"))
                    .unwrap());
                assert!(!reopened.is_favourite(&root.join("liked 猫.svg")).unwrap());
            }
        }
    }

    #[test]
    fn organising_images_renames_destination_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let state = StateStore::new(None).unwrap();
        fs::create_dir(root.join("favourites")).unwrap();
        fs::write(root.join("favourites/cat.svg"), "existing image").unwrap();
        fs::write(root.join("favourites/cat_2.svg"), "second image").unwrap();
        fs::write(root.join("cat.svg"), "new image").unwrap();
        fs::write(root.join("dog.svg"), "another image").unwrap();
        state.set_favourite(&root.join("cat.svg"), true).unwrap();
        state.set_favourite(&root.join("dog.svg"), true).unwrap();

        let result = move_favourite_images(&root, &state).unwrap();
        assert_eq!(result.moved, 2);
        assert!(result.errors.is_empty());
        assert_eq!(
            fs::read_to_string(root.join("favourites/cat.svg")).unwrap(),
            "existing image"
        );
        assert_eq!(
            fs::read_to_string(root.join("favourites/cat_2.svg")).unwrap(),
            "second image"
        );
        assert_eq!(
            fs::read_to_string(root.join("favourites/cat_3.svg")).unwrap(),
            "new image"
        );
        assert!(!root.join("cat.svg").exists());
        assert!(state
            .is_favourite(&root.join("favourites/cat_3.svg"))
            .unwrap());
        assert!(state
            .is_favourite(&root.join("favourites/dog.svg"))
            .unwrap());
    }

    #[test]
    fn gallery_comment_endpoint_shares_edits_and_deletes_across_images() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("first.svg"), "<svg>first</svg>").unwrap();
        fs::write(directory.path().join("second.svg"), "<svg>second</svg>").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let state = StateStore::new(None).unwrap();
            for _ in 0..8 {
                let (stream, _) = listener.accept().unwrap();
                handle_connection(
                    stream,
                    &root,
                    &CollaborationHub::default(),
                    &ReviewHub::default(),
                    false,
                    None,
                    &state,
                )
                .unwrap();
            }
        });
        let body = |response: String| response.split_once("\r\n\r\n").unwrap().1.to_owned();

        let empty = test_http_request(address, "GET", "/first.svg?mode=gallery-comments", "");
        assert!(empty.starts_with("HTTP/1.1 200 OK"));
        assert!(
            serde_json::from_str::<serde_json::Value>(&body(empty)).unwrap()["comments"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let add = serde_json::json!({
            "type": "add",
            "comment": {
                "id": "one",
                "image": "first.svg",
                "author": "Alice",
                "body": "降低背景亮度",
                "created_at": "2026-09-09T10:00:00.000Z"
            }
        })
        .to_string();
        assert!(
            test_http_request(address, "POST", "/first.svg?mode=gallery-comments", &add)
                .starts_with("HTTP/1.1 200 OK")
        );

        let shared = body(test_http_request(
            address,
            "GET",
            "/second.svg?mode=gallery-comments",
            "",
        ));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&shared).unwrap()["comments"][0]["image"],
            "first.svg"
        );

        let edit = serde_json::json!({
            "type": "edit",
            "comment_id": "one",
            "body": "降低背景高光",
            "edited_at": "2026-09-09T11:00:00.000Z"
        })
        .to_string();
        let edited = body(test_http_request(
            address,
            "POST",
            "/second.svg?mode=gallery-comments",
            &edit,
        ));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&edited).unwrap()["comments"][0]["body"],
            "降低背景高光"
        );

        let add_second = serde_json::json!({
            "type": "add",
            "comment": {
                "id": "two",
                "image": "second.svg",
                "author": "Alice",
                "body": "保留这条评论",
                "created_at": "2026-09-09T12:00:00.000Z"
            }
        })
        .to_string();
        assert!(test_http_request(
            address,
            "POST",
            "/second.svg?mode=gallery-comments",
            &add_second
        )
        .starts_with("HTTP/1.1 200 OK"));

        let cleared = body(test_http_request(
            address,
            "POST",
            "/first.svg?mode=gallery-comments",
            r#"{"type":"delete-all"}"#,
        ));
        let cleared = serde_json::from_str::<serde_json::Value>(&cleared).unwrap();
        assert_eq!(cleared["comments"].as_array().unwrap().len(), 1);
        assert_eq!(cleared["comments"][0]["image"], "second.svg");

        let delete = serde_json::json!({"type": "delete", "comment_id": "two"}).to_string();
        let deleted = body(test_http_request(
            address,
            "POST",
            "/second.svg?mode=gallery-comments",
            &delete,
        ));
        assert!(
            serde_json::from_str::<serde_json::Value>(&deleted).unwrap()["comments"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let listing = test_http_request(address, "GET", "/?view=gallery", "");
        assert!(!body(listing).contains("href=\"/.gallery-comments.json\""));
        server.join().unwrap();
    }

    #[test]
    fn favourite_requests_update_directory_order_and_preserve_the_image() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        fs::write(root.join("a.svg"), "<svg></svg>").unwrap();
        fs::write(root.join("z.svg"), "<svg></svg>").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let state = StateStore::new(None).unwrap();
            for _ in 0..4 {
                let (stream, _) = listener.accept().unwrap();
                handle_connection(
                    stream,
                    &root,
                    &CollaborationHub::default(),
                    &ReviewHub::default(),
                    false,
                    None,
                    &state,
                )
                .unwrap();
            }
        });
        let request = |method: &str, target: &str| {
            let mut client = TcpStream::connect(address).unwrap();
            write!(
                client,
                "{method} {target} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            response
        };
        assert!(request("PUT", "/z.svg?mode=favourite").starts_with("HTTP/1.1 204"));
        let liked = request("GET", "/?view=gallery");
        assert!(liked.find("href=\"/z.svg\"").unwrap() < liked.find("href=\"/a.svg\"").unwrap());
        assert!(liked.contains("data-favourite=\"true\""));
        assert!(liked.contains("class=\"favourite-mark\" title=\"已点赞\"><svg"));
        assert!(request("DELETE", "/z.svg?mode=favourite").starts_with("HTTP/1.1 204"));
        let unliked = request("GET", "/");
        assert!(
            unliked.find("href=\"/a.svg\"").unwrap() < unliked.find("href=\"/z.svg\"").unwrap()
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("z.svg")).unwrap(),
            "<svg></svg>"
        );
        server.join().unwrap();
    }

    #[test]
    fn image_heavy_directory_offers_gallery_switch() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        image::RgbImage::from_pixel(24, 16, image::Rgb([40, 90, 180]))
            .save(root.join("photo.png"))
            .unwrap();
        fs::write(
            root.join("icon.svg"),
            "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        )
        .unwrap();
        fs::write(root.join("notes.md"), "# hi\n").unwrap();
        fs::create_dir(root.join("docs")).unwrap();

        let page = render_directory_page(root, root, &StateStore::new(None).unwrap()).unwrap();

        assert!(page.contains("<body>"));
        assert!(page.contains("class=\"listing\""));
        assert!(!page.contains("class=\"listing gallery\""));
        assert!(page.contains("id=\"gallery-toggle\""));
        assert!(page.contains("aria-pressed=\"false\""));
        assert!(page.contains("HTTP / DIRECTORY"));
        assert!(page.contains("class=\"entry file image\""));
        assert!(page.contains("class=\"entry file image vector\""));
        assert!(page.contains("src=\"/photo.png?mode=thumb&amp;v="));
        assert!(page.contains("data-gallery-src=\"/photo.png?mode=gallery-thumb&amp;v="));
        assert!(page.contains("data-preview-src=\"/photo.png?mode=gallery-preview&amp;v="));
        assert!(page.contains("data-original-src=\"/photo.png?mode=asset&amp;v="));
        assert!(page.contains("src=\"/icon.svg?mode=asset&amp;v="));
        assert!(page.contains("class=\"entry file\" href=\"/notes.md\""));
        assert!(!page.contains("notes.md?mode=thumb"));
        assert!(page.contains("class=\"entry folder\" href=\"/docs/\""));
        assert!(page.contains("data-list-src=\"/photo.png?mode=thumb&amp;v="));
        assert!(
            page.contains(".entry.image .glyph img { width:100%; height:100%; object-fit:cover;")
        );
        assert!(page.contains(".listing.gallery { display:grid;"));
        assert!(page.contains("listing.classList.toggle('gallery', enabled)"));
        assert!(page.contains("data-gallery-href=\"/photo.png?view=gallery\""));
        assert!(page.contains("url.searchParams.set('view', 'gallery')"));
        assert!(page.contains(
            "setGallery(new URLSearchParams(location.search).get('view') === 'gallery', false)"
        ));
        assert!(page.contains("id=\"image-lightbox\""));
        assert!(page.contains("id=\"scroll-jumps\""));
        assert!(page.contains("id=\"scroll-to-top\""));
        assert!(page.contains("id=\"scroll-to-bottom\""));
        assert!(page.contains("new ResizeObserver(scheduleScrollJumpUpdate)"));
        assert!(page.contains("id=\"lightbox-position\""));
        assert!(page.contains("id=\"lightbox-filmstrip\""));
        assert!(page.contains("id=\"preview-previous\""));
        assert!(page.contains("const preloadAdjacentImages = () =>"));
        assert!(page.contains("preloadAdjacentImages();"));
        assert!(page.contains("id=\"preview-next\""));
        assert!(page.contains("id=\"deletion-toggle\""));
        assert!(page.contains("id=\"favourite-burst\""));
        assert!(page.contains("id=\"carousel-toggle\""));
        assert!(page.contains("commentToggle.className = 'comment-toggle'"));
        assert!(page.contains("viewerControls.insertBefore(commentToggle, deletionToggle)"));
        assert!(page.contains("viewerExtras.hidden = true"));
        assert!(page.contains("if (event.key.toLowerCase() === 'c')"));
        assert!(page.contains("?mode=gallery-comments"));
        assert!(page.contains("class=\"gallery-comment-composer\""));
        assert!(page.contains("data-comment-edit"));
        assert!(page.contains("data-comment-delete"));
        assert!(page.contains("data-comment-delete-all"));
        assert!(page.contains("submitGalleryCommentAction({type: 'delete-all'})"));
        assert!(page.contains(
            "const carouselEffects = ['drift', 'lift', 'depth', 'soft-focus', 'curtain']"
        ));
        assert!(page.contains("carouselTimer = setTimeout(advanceCarousel, 5000)"));
        assert!(page.contains("carouselQueue = shuffle"));
        assert!(page.contains("if (event.key === 'p')"));
        assert!(page.contains("image-lightbox.carousel-mode"));
        assert!(page.contains("document.addEventListener('visibilitychange'"));
        assert!(page.contains("deletionToggle.innerHTML = svgIcon"));
        assert!(page.contains("marked ? '取消标记 (m)' : '标记删除 (m)'"));
        assert!(page.contains("id=\"deletion-filter\""));
        assert!(page.contains("id=\"delete-marked-images\""));
        assert!(page.contains(
            "id=\"delete-marked-images\" class=\"delete-marked-images\" type=\"button\" hidden"
        ));
        assert!(page.contains("id=\"clear-deletion-marks\" type=\"button\" hidden"));
        assert!(page.contains("?mode=clear-deletion-marks"));
        assert!(page.contains("moveFavouriteButton.hidden = enabled"));
        assert!(page.contains("id=\"delete-marked-dialog\""));
        assert!(page.contains("if (event.key === 'm')"));
        assert!(page.contains("?mode=deletion-mark"));
        assert!(page.contains("?mode=delete-marked"));
        assert!(page.contains("lightboxClose.addEventListener('click', closeLightbox)"));
        assert!(page.contains("lightbox.addEventListener('pointermove', updateMouseChrome"));
        assert!(page.contains("mouseInChromeZone = true"));
        assert!(page.contains("showChrome(true)"));
        assert!(page.contains("if (opening) {"));
        assert!(page.contains("lightbox.classList.add('chrome-hidden')"));
        assert!(page.contains("if (!gesture.imageTap) {"));
        assert!(
            page.contains("if (lightbox.classList.contains('carousel-mode')) setCarousel(false)")
        );
        assert!(page.contains("lightbox.addEventListener('pointerdown'"));
        assert!(page.contains("stepPreview(delta < 0 ? 1 : -1, delta)"));
        assert!(page.contains("const doubleTap = lastImageTap"));
        assert!(page.contains("imageTap: event.target === lightboxImage"));
        assert!(page.contains("if (gesture.moved) return"));
        assert!(page.contains("animateFavouriteFeedback(liked, origin)"));
        assert!(!page.contains("longPressTimer"));
        assert!(page.contains("const animatePreview = (outgoing, outgoingBackdrop, direction, effect, gestureOffset = null, gestureIncoming = null) =>"));
        assert!(page.contains("lightboxBackdrop.className = 'carousel-backdrop'"));
        assert!(page.contains("background-size:cover"));
        assert!(page.contains("filter:blur(12px) brightness(.4) saturate(1.06)"));
        assert!(page.contains("outgoingBackdropAnimation"));
        assert!(page.contains("const vertical = mobileTouch.matches"));
        assert!(page.contains("`${index + 1} / ${entries.length}`"));
        assert!(page.contains("for (let offset = -3; offset <= 3; offset++)"));
        assert!(page.contains("button.addEventListener('click', () => switchPreview"));
        assert!(page.contains("touch-action:none"));
        assert!(page.contains("position:absolute; z-index:1; inset:0; display:block; width:100%; height:100%; object-fit:scale-down"));
    }

    #[test]
    fn gallery_comment_editor_owns_keyboard_input_and_shares_review_identity() {
        let directory = tempfile::tempdir().unwrap();
        image::RgbImage::from_pixel(24, 16, image::Rgb([40, 90, 180]))
            .save(directory.path().join("photo.png"))
            .unwrap();
        let page = render_directory_page(
            directory.path(),
            directory.path(),
            &StateStore::new(None).unwrap(),
        )
        .unwrap();
        let markdown = render_markdown_page("# Review", "review.md");

        assert!(page.contains("const REVIEW_IDENTITY_KEY = 'http-file-server-review-identity'"));
        assert!(markdown.contains("const REVIEW_IDENTITY_KEY = 'http-file-server-review-identity'"));
        assert!(!page.contains("http-gallery-comment-author"));
        assert!(page.contains("const commentEditorActive = !commentsDrawer.hidden"));
        assert!(page.contains(
            "event.target === galleryCommentAuthor || event.target === galleryCommentBody"
        ));
        assert!(page.contains("galleryCommentComposer.requestSubmit()"));
        assert!(page.contains("if (event.metaKey)"));
        assert!(page.contains("galleryCommentBody.setRangeText('\\n'"));
        assert!(page.contains("Enter 提交 · ⌘ Enter 换行"));
    }

    #[test]
    fn gallery_fullscreen_swipe_and_favourite_effect_contracts_are_embedded() {
        let directory = tempfile::tempdir().unwrap();
        image::RgbImage::from_pixel(24, 16, image::Rgb([40, 90, 180]))
            .save(directory.path().join("photo.png"))
            .unwrap();
        let page = render_directory_page(
            directory.path(),
            directory.path(),
            &StateStore::new(None).unwrap(),
        )
        .unwrap();

        assert!(page.contains("element.requestFullscreen || element.webkitRequestFullscreen"));
        assert!(page.contains("document.exitFullscreen || document.webkitExitFullscreen"));
        assert!(page.contains("document.addEventListener('webkitfullscreenchange'"));
        assert!(page.contains(".image-lightbox:-webkit-full-screen"));
        assert!(page.contains("if (stepPreview(delta < 0 ? 1 : -1, delta)) return"));
        assert!(page.contains("const incomingTarget = gestureIncoming || lightboxImage"));
        assert!(page.contains("lightbox.append(heartParticles)"));
        assert!(page.contains("const particleRect = lightbox.getBoundingClientRect()"));
    }

    #[test]
    fn directory_hides_review_and_gallery_comment_sidecars() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("spec.md"), "# Spec").unwrap();
        fs::write(
            directory.path().join("spec.md.review.json"),
            r#"{"version":1,"comments":[]}"#,
        )
        .unwrap();
        fs::write(
            directory.path().join(".gallery-comments.json"),
            r#"{"version":1,"comments":[]}"#,
        )
        .unwrap();

        let page = render_directory_page(
            directory.path(),
            directory.path(),
            &StateStore::new(None).unwrap(),
        )
        .unwrap();

        assert!(page.contains("spec.md"));
        assert!(!page.contains("spec.md.review.json"));
        assert!(!page.contains("href=\"/.gallery-comments.json\""));
        assert!(page.contains("1 个文件"));
    }

    #[test]
    fn review_websocket_broadcasts_actions_and_named_presence() {
        use tungstenite::{connect, Message};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("spec.md");
        fs::write(&path, "# Spec").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let collaboration = CollaborationHub::default();
        let reviews = ReviewHub::default();
        let server = std::thread::spawn(move || {
            let mut connections = Vec::new();
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                let root = root.clone();
                let collaboration = collaboration.clone();
                let reviews = reviews.clone();
                connections.push(std::thread::spawn(move || {
                    handle_connection(
                        stream,
                        &root,
                        &collaboration,
                        &reviews,
                        false,
                        None,
                        &StateStore::new(None).unwrap(),
                    )
                    .unwrap();
                }));
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });

        let url = format!("ws://{address}/spec.md?mode=review-collab");
        let (mut alice, _) = connect(&url).unwrap();
        alice
            .send(Message::Text(
                serde_json::json!({"type": "join", "user": "alice@laptop"})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        let (mut bob, _) = connect(&url).unwrap();
        bob.send(Message::Text(
            serde_json::json!({"type": "join", "user": "bob@desktop"})
                .to_string()
                .into(),
        ))
        .unwrap();

        for socket in [&mut alice, &mut bob] {
            loop {
                if let Message::Text(message) = socket.read().unwrap() {
                    if message.contains("alice@laptop") && message.contains("bob@desktop") {
                        break;
                    }
                }
            }
        }

        let action = serde_json::json!({
            "type": "add-comment",
            "comment": {
                "id": "review-one",
                "scope": {"type": "document"},
                "status": "open",
                "messages": [{
                    "author": "alice@laptop",
                    "body": "补充失败策略",
                    "created_at": "2026-09-04T08:00:00.000Z"
                }]
            }
        })
        .to_string();
        let mut client = TcpStream::connect(address).unwrap();
        write!(
            client,
            "POST /spec.md?mode=review-action HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{action}",
            action.len()
        )
        .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("review-one"));

        for socket in [&mut alice, &mut bob] {
            loop {
                if let Message::Text(message) = socket.read().unwrap() {
                    if message.contains("review-updated") {
                        break;
                    }
                }
            }
        }
        alice.close(None).unwrap();
        bob.close(None).unwrap();
        server.join().unwrap();
        assert!(fs::read_to_string(review::sidecar_path(&path))
            .unwrap()
            .contains("review-one"));
    }

    #[test]
    fn gallery_is_available_when_directory_contains_any_image() {
        assert!(should_use_gallery(3, 5));
        assert!(should_use_gallery(4, 5));
        assert!(should_use_gallery(1, 5));
        assert!(!should_use_gallery(0, 5));
        assert!(!should_use_gallery(0, 0));
    }

    #[test]
    fn regular_directory_keeps_the_compact_listing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        image::RgbImage::from_pixel(24, 16, image::Rgb([40, 90, 180]))
            .save(root.join("photo.png"))
            .unwrap();
        fs::write(root.join("one.txt"), "one").unwrap();
        fs::write(root.join("two.txt"), "two").unwrap();

        let page = render_directory_page(root, root, &StateStore::new(None).unwrap()).unwrap();

        assert!(page.contains("<body>"));
        assert!(page.contains("class=\"listing\""));
        assert!(!page.contains("class=\"listing gallery\""));
        assert!(page.contains("id=\"gallery-toggle\""));
        assert!(page.contains("data-gallery-src="));
        assert!(page.contains("src=\"/photo.png?mode=thumb&amp;v="));
    }

    #[test]
    fn generates_a_downscaled_thumbnail_for_raster_images() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wide.png");
        image::RgbImage::from_pixel(800, 200, image::Rgb([30, 80, 180]))
            .save(&path)
            .unwrap();

        let (bytes, content_type) =
            render_image_thumbnail(&fs::read(&path).unwrap(), THUMBNAIL_MAX_EDGE).unwrap();
        assert_eq!(content_type, "image/jpeg");
        let thumb = image::load_from_memory(&bytes).unwrap();
        assert_eq!(thumb.width(), 128);
        assert_eq!(thumb.height(), 32);

        let (bytes, _) =
            render_image_thumbnail(&fs::read(&path).unwrap(), GALLERY_THUMBNAIL_MAX_EDGE).unwrap();
        let gallery_thumb = image::load_from_memory(&bytes).unwrap();
        assert_eq!(gallery_thumb.width(), 512);
        assert_eq!(gallery_thumb.height(), 128);

        let (bytes, _) =
            render_image_thumbnail(&fs::read(&path).unwrap(), GALLERY_PREVIEW_MAX_EDGE).unwrap();
        let gallery_preview = image::load_from_memory(&bytes).unwrap();
        assert_eq!(gallery_preview.width(), 800);
        assert_eq!(gallery_preview.height(), 200);

        let transparent = directory.path().join("alpha.png");
        image::RgbaImage::from_pixel(64, 64, image::Rgba([12, 24, 48, 80]))
            .save(&transparent)
            .unwrap();
        let (bytes, content_type) =
            render_image_thumbnail(&fs::read(&transparent).unwrap(), THUMBNAIL_MAX_EDGE).unwrap();
        assert_eq!(content_type, "image/png");
        assert!(image::load_from_memory(&bytes).unwrap().color().has_alpha());
    }

    #[test]
    fn thumbnail_url_skips_non_images_and_huge_files() {
        assert_eq!(
            thumbnail_url(
                "/photo.png",
                Path::new("photo.png"),
                1024,
                false,
                Some("v1")
            )
            .as_deref(),
            Some("/photo.png?mode=thumb&amp;v=v1")
        );
        assert_eq!(
            thumbnail_url("/photo.png", Path::new("photo.png"), 1024, true, Some("v1")).as_deref(),
            Some("/photo.png?mode=gallery-thumb&amp;v=v1")
        );
        assert_eq!(
            thumbnail_url("/logo.svg", Path::new("logo.svg"), 2048, true, Some("v1")).as_deref(),
            Some("/logo.svg?mode=asset&amp;v=v1")
        );
        assert_eq!(
            thumbnail_url("/notes.md", Path::new("notes.md"), 128, false, Some("v1")),
            None
        );
        assert_eq!(
            thumbnail_url(
                "/photo.png",
                Path::new("photo.png"),
                MAX_THUMBNAIL_SOURCE + 1,
                true,
                Some("v1")
            ),
            None
        );
    }

    #[test]
    fn gallery_preview_url_uses_screen_derivative_and_keeps_special_images_original() {
        assert_eq!(
            gallery_preview_url("/photo.jpg", Path::new("photo.jpg"), 1024, Some("v1")).as_deref(),
            Some("/photo.jpg?mode=gallery-preview&amp;v=v1")
        );
        assert_eq!(
            gallery_preview_url("/movie.gif", Path::new("movie.gif"), 1024, Some("v1")),
            None
        );
        assert_eq!(
            gallery_preview_url("/logo.svg", Path::new("logo.svg"), 1024, Some("v1")),
            None
        );
    }

    #[test]
    fn renders_svg_in_an_isolated_image_preview() {
        let page = render_svg_page("icon<&>.svg", 1536);

        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("src=\"?mode=asset\""));
        assert!(page.contains("href=\"?mode=raw\""));
        assert!(page.contains("id=\"history-back\" type=\"button\" aria-label=\"返回上一页\""));
        assert!(page.contains("class=\"canvas\""));
        assert!(page.contains("icon&lt;&amp;&gt;.svg"));
        assert!(!page.contains("icon<&>.svg"));
    }

    #[test]
    fn renders_raster_images_with_a_history_back_button() {
        let page = render_image_page("photo<&>.png", "PNG", 2048);

        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("id=\"history-back\" type=\"button\" aria-label=\"返回上一页\""));
        assert!(page.contains("src=\"?mode=asset\""));
        assert!(page.contains(">PNG</span>"));
        assert!(page.contains("photo&lt;&amp;&gt;.png"));
        assert!(!page.contains("photo<&>.png"));
    }

    #[test]
    fn renders_drawio_with_the_bundled_offline_viewer() {
        let diagram = r#"<mxGraphModel><root><mxCell id="0" value="</div><script>alert(1)</script>"/></root></mxGraphModel>"#;
        let page = render_drawio_page(diagram, "system<&>.drawio", 2048);

        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("class=\"mxgraph\""));
        assert!(page.contains(DRAWIO_VIEWER_PATH));
        assert!(page.contains("connect-src 'none'"));
        assert!(page.contains("toolbar&quot;:&quot;zoom layers lightbox"));
        assert!(page.contains("system&lt;&amp;&gt;.drawio"));
        assert!(!page.contains("<script>alert(1)</script>"));
        assert!(!page.contains("https://viewer.diagrams.net"));
        assert!(DRAWIO_VIEWER_JS.len() > 4_000_000);
        assert_eq!(
            mime_type(Path::new("system.drawio")),
            "application/vnd.jgraph.mxfile"
        );
        assert_eq!(file_kind(Path::new("system.drawio")), "DRAWIO");
    }

    #[test]
    fn highlights_common_programming_languages() {
        let samples = [
            ("main.rs", "fn main() { println!(\"hello\"); }\n"),
            ("schema.sql", "SELECT id FROM users WHERE active = true;\n"),
            ("data.json", "{\"name\": \"http\", \"port\": 8080}\n"),
            ("events.jsonl", "{\"event\": \"started\"}\n"),
            ("main.go", "package main\nfunc main() {}\n"),
            ("app.js", "const answer = () => 42;\n"),
            ("app.ts", "const answer: number = 42;\n"),
            ("run.sh", "#!/bin/bash\necho hello\n"),
            ("core.lisp", "(defun square (x) (* x x))\n"),
            ("app.py", "def hello(name: str):\n    return f'Hi {name}'\n"),
            (
                "App.java",
                "class App { public static void main(String[] a) {} }\n",
            ),
            ("main.c", "int main(void) { return 0; }\n"),
            (
                "main.cpp",
                "#include <iostream>\nint main() { return 0; }\n",
            ),
        ];

        for (path, source) in samples {
            let highlighted = highlighted_source_lines(source, Path::new(path));
            assert!(highlighted.is_some(), "expected highlighting for {path}");
            assert!(highlighted.unwrap().contains("style=\""), "{path}");
        }
    }

    #[test]
    fn only_renders_text_for_document_requests() {
        assert!(request_wants_html(&RequestHeaders {
            accept: "text/html,application/xhtml+xml".into(),
            ..RequestHeaders::default()
        }));
        assert!(!request_wants_html(&RequestHeaders {
            accept: "*/*".into(),
            fetch_dest: "script".into(),
            ..RequestHeaders::default()
        }));
    }
}
