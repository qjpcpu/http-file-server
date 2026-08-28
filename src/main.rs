use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use pulldown_cmark::{html, Event, Options, Parser};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::html::{styled_line_to_highlighted_html, IncludeBackground};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

const DEFAULT_PORT: u16 = 8080;
const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;
const MAX_TEXT_VIEWER_FILE: u64 = 4 * 1024 * 1024;
const MAX_DRAWIO_VIEWER_FILE: u64 = 16 * 1024 * 1024;
const DRAWIO_VIEWER_PATH: &str = "/__http_file_server/drawio-viewer-31.3.1.js";
const DRAWIO_VIEWER_JS: &[u8] = include_bytes!("../assets/drawio-viewer-static-31.3.1.min.js");

#[derive(Debug, PartialEq)]
struct PortConfig {
    port: u16,
    fallback_to_random: bool,
}

#[derive(Default)]
struct RequestHeaders {
    content_length: usize,
    accept: String,
    fetch_dest: String,
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

    let root = match env::current_dir().and_then(fs::canonicalize) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("无法读取当前目录: {error}");
            std::process::exit(1);
        }
    };
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

    println!("Serving {} at http://localhost:{port}", root.display());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let root = root.clone();
                std::thread::spawn(move || {
                    if let Err(error) = handle_connection(stream, &root) {
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
    "用法: http [-p PORT]\n\n选项:\n  -p, --port PORT  指定监听端口（默认 8080）\n  -h, --help       显示帮助"
}

fn handle_connection(mut stream: TcpStream, root: &Path) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let head_only = method == "HEAD";

    if !matches!(method, "GET" | "HEAD" | "POST" | "PUT") {
        return send_text(
            &mut stream,
            405,
            "Method Not Allowed",
            "仅支持 GET、HEAD、POST 和 PUT\n",
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
    if matches!(method, "GET" | "HEAD") && request_path == DRAWIO_VIEWER_PATH {
        return send_content(
            &mut stream,
            DRAWIO_VIEWER_JS,
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
    let raw_mode = mode.as_deref() == Some("raw");
    let preview_mode = mode.as_deref() == Some("preview");
    let asset_mode = mode.as_deref() == Some("asset");
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
    if metadata.is_dir() {
        if !request_path.ends_with('/') {
            let location = if query.is_empty() {
                format!("{request_path}/")
            } else {
                format!("{request_path}/?{query}")
            };
            return send_redirect(&mut stream, &location);
        }
        let body = render_directory_page(root, &canonical)?;
        return send_html(&mut stream, &body, head_only);
    }
    if !metadata.is_file() {
        let body = render_not_found_page(&decoded);
        return send_html_status(&mut stream, 404, "Not Found", &body, head_only);
    }

    if method == "POST" {
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
        fs::write(&canonical, markdown.as_bytes())?;
        return send_empty(&mut stream, 204, "No Content");
    }

    // Image previews load the original asset through this internal URL so SVG
    // markup is never injected into the viewer document.
    if asset_mode {
        return send_file(
            &mut stream,
            &canonical,
            metadata.len(),
            mime_type(&canonical),
            head_only,
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
        return send_html(&mut stream, &body, head_only);
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
        return send_html(&mut stream, &body, head_only);
    }

    if has_extension(&canonical, "md") {
        let markdown = fs::read_to_string(&canonical)?;
        let title = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Markdown");
        let body = render_markdown_page(&markdown, title);
        return send_html(&mut stream, &body, head_only);
    }

    if request_wants_html(&headers) && metadata.len() <= MAX_TEXT_VIEWER_FILE {
        if let Some(text_file) = read_text_file(&canonical, metadata.len())? {
            let body = render_text_page(
                &text_file.content,
                &text_file.title,
                text_file.kind,
                &canonical,
            );
            return send_html(&mut stream, &body, head_only);
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
            } else if name.eq_ignore_ascii_case("accept") {
                headers.accept = value.trim().to_ascii_lowercase();
            } else if name.eq_ignore_ascii_case("sec-fetch-dest") {
                headers.fetch_dest = value.trim().to_ascii_lowercase();
            }
        }
    }
    Ok(headers)
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

fn render_directory_page(root: &Path, directory: &Path) -> io::Result<String> {
    let relative = directory.strip_prefix(root).unwrap_or(Path::new(""));
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let metadata = entry.metadata().ok();
        entries.push(DirectoryEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path(),
            is_dir: metadata
                .as_ref()
                .map_or_else(|| file_type.is_dir(), fs::Metadata::is_dir),
            size: metadata.as_ref().map_or(0, fs::Metadata::len),
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });

    let directory_count = entries.iter().filter(|entry| entry.is_dir).count();
    let file_count = entries.len() - directory_count;
    let title = relative
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("根目录");

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
        let class = if entry.is_dir { "folder" } else { "file" };
        rows.push_str(&format!(
            "<a class=\"entry {class}\" href=\"{href}\"><span class=\"glyph\" aria-hidden=\"true\"></span><span class=\"entry-name\">{name}</span><span class=\"kind\">{kind}</span><span class=\"detail\">{detail}</span><span class=\"arrow\" aria-hidden=\"true\">→</span></a>"
        ));
    }
    if rows.is_empty() {
        rows.push_str("<div class=\"empty\"><span>∅</span><p>这个目录是空的</p></div>");
    }

    let title = escape_html(title);
    Ok(format!(
        "<!doctype html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\n<title>{title} · 文件浏览</title>\n<style>{DIRECTORY_CSS}</style>\n</head>\n<body>\n<main><nav class=\"breadcrumbs\" aria-label=\"当前位置\">{breadcrumbs}</nav><header><p class=\"eyebrow\">HTTP / DIRECTORY</p><h1>{title}</h1><p class=\"summary\">{directory_count} 个目录 · {file_count} 个文件</p></header><section class=\"listing\" aria-label=\"目录内容\">{rows}</section></main>\n</body>\n</html>"
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
        "<!doctype html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><title>{title}</title><style>{TEXT_CSS}</style></head><body{body_class}><header><a class=\"back\" href=\"./\" aria-label=\"返回目录\">←</a><span class=\"kind\">{kind}</span><span class=\"filename\">{title}</span><span class=\"meta\">{line_count} 行 · {size}</span><button id=\"wrap\" type=\"button\">自动换行</button><button id=\"copy\" type=\"button\">复制</button><a class=\"raw\" href=\"?mode=raw\">Raw</a></header><main><pre id=\"code\"><code>{lines}</code></pre></main><div id=\"toast\" role=\"status\"></div><script>{TEXT_JS}</script></body></html>",
        size = human_size(content.len() as u64)
    )
}

fn render_svg_page(title: &str, size: u64) -> String {
    let title = escape_html(title);
    format!(
        "<!doctype html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><title>{title}</title><style>{SVG_CSS}</style></head><body><header><a class=\"back\" href=\"./\" aria-label=\"返回目录\">←</a><span class=\"kind\">SVG</span><span class=\"filename\">{title}</span><span class=\"meta\">{size}</span><button id=\"scale\" type=\"button\">原始尺寸</button><a class=\"raw\" href=\"?mode=raw\">Raw</a></header><main class=\"canvas\"><img id=\"artwork\" src=\"?mode=asset\" alt=\"{title}\"><p id=\"error\" hidden>无法渲染这个 SVG 文件</p></main><script>{SVG_JS}</script></body></html>",
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
        "<!doctype html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'self' data: blob:; connect-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; object-src 'none'; base-uri 'none'; frame-src 'none'; worker-src 'self' blob:\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><title>{title}</title><style>{DRAWIO_CSS}</style></head><body><header><a class=\"back\" href=\"./\" aria-label=\"返回目录\">←</a><span class=\"kind\">DRAWIO</span><span class=\"filename\">{title}</span><span class=\"meta\">{size}</span><a class=\"raw\" href=\"?mode=raw\">Raw</a></header><main class=\"canvas\"><div id=\"viewer-status\" class=\"viewer-status\"><span class=\"spinner\" aria-hidden=\"true\"></span><strong>正在渲染图表</strong><small>本地 Draw.io Viewer</small></div><div class=\"mxgraph\" data-mxgraph=\"{config}\"></div></main><script>{DRAWIO_JS}</script><script src=\"{viewer_path}\" onerror=\"drawioViewerFailed()\"></script></body></html>",
        size = human_size(size),
        viewer_path = DRAWIO_VIEWER_PATH
    )
}

fn render_markdown_page(markdown: &str, title: &str) -> String {
    let article = render_markdown_article(markdown);
    let title = escape_html(title);
    let source = escape_html(markdown);
    format!(
        "<!doctype html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\n<title>{title}</title>\n<style>{MARKDOWN_CSS}</style>\n</head>\n<body>\n<header class=\"topbar\"><a class=\"home\" href=\"./\" aria-label=\"返回目录\">←</a><span class=\"mark\">MD</span><span class=\"filename\">{title}</span><span class=\"top-actions\"><a class=\"button ghost\" href=\"?mode=raw\">Raw</a><button class=\"button\" id=\"edit-button\" type=\"button\">编辑</button></span></header>\n<div id=\"reader\" class=\"reader-layout\"><aside id=\"toc\" aria-label=\"文档目录\"></aside><main class=\"paper\"><article id=\"article\">{article}</article></main></div>\n<section id=\"editor\" class=\"editor-shell\" hidden><div class=\"editor-toolbar\"><div class=\"format-tools\" role=\"toolbar\" aria-label=\"Markdown 格式\"><button type=\"button\" data-format=\"heading\" title=\"标题\">H</button><button type=\"button\" data-format=\"bold\" title=\"粗体\"><strong>B</strong></button><button type=\"button\" data-format=\"italic\" title=\"斜体\"><em>I</em></button><button type=\"button\" data-format=\"link\" title=\"链接\">↗</button><button type=\"button\" data-format=\"quote\" title=\"引用\">❯</button><button type=\"button\" data-format=\"code\" title=\"代码\">&lt;/&gt;</button><button type=\"button\" data-format=\"list\" title=\"列表\">≡</button><button type=\"button\" data-format=\"task\" title=\"任务\">☑</button></div><span id=\"save-status\" role=\"status\"></span><button class=\"button ghost\" id=\"cancel-button\" type=\"button\">取消</button><button class=\"button\" id=\"save-button\" type=\"button\">保存</button></div><div class=\"editor-panes\"><div class=\"pane preview-pane\"><span>PREVIEW</span><iframe id=\"preview\" title=\"Markdown 实时预览\"></iframe></div><label class=\"pane source-pane\"><span>MARKDOWN</span><textarea id=\"source\" spellcheck=\"false\">{source}</textarea></label></div></section>\n<dialog id=\"discard-dialog\" class=\"confirm-dialog\" aria-labelledby=\"discard-title\" aria-describedby=\"discard-description\"><span class=\"dialog-mark\" aria-hidden=\"true\">!</span><h2 id=\"discard-title\">放弃这次修改？</h2><p id=\"discard-description\">尚未保存的 Markdown 内容将会丢失，且无法恢复。</p><div class=\"dialog-actions\"><button class=\"button ghost\" id=\"keep-editing-button\" type=\"button\">继续编辑</button><button class=\"button danger\" id=\"discard-button\" type=\"button\">放弃修改</button></div></dialog>\n<script>{MARKDOWN_JS}</script>\n</body>\n</html>"
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
    let parser = Parser::new_ext(markdown, markdown_options()).map(|event| match event {
        Event::Html(value) | Event::InlineHtml(value) => Event::Text(value),
        event => event,
    });
    let mut article = String::new();
    html::push_html(&mut article, parser);
    article
}

fn render_markdown_preview_page(markdown: &str) -> String {
    let article = render_markdown_article(markdown);
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><style>{MARKDOWN_CSS}</style></head><body class=\"preview-body\"><main class=\"paper preview-paper\"><article>{article}</article></main></body></html>"
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
:root { color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#272a38; --muted:#73788b; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --gutter:#f0f2f8; }
* { box-sizing:border-box; }
html,body { min-height:100%; }
body { margin:0; color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC",sans-serif; }
header { position:sticky; top:0; z-index:2; display:flex; align-items:center; gap:.65rem; min-height:3.6rem; padding:.7rem max(1rem,calc((100vw - 1280px)/2)); border-bottom:1px solid var(--line); background:color-mix(in srgb,var(--paper) 90%,transparent); backdrop-filter:blur(16px); }
.back { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.5rem; color:var(--muted); text-decoration:none; }
.back:hover { color:var(--accent); background:var(--accent-soft); }
.kind { flex:0 0 auto; padding:.28rem .5rem; border-radius:.35rem; color:white; background:var(--accent); font:750 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.05em; }
.filename { overflow:hidden; font:650 .82rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; }
.meta { margin-left:auto; color:var(--muted); font:500 .7rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; white-space:nowrap; }
button,.raw { min-height:2rem; padding:.45rem .65rem; border:1px solid var(--line); border-radius:.45rem; color:var(--ink); background:var(--surface); font:650 .72rem/1 ui-sans-serif,-apple-system,sans-serif; text-decoration:none; cursor:pointer; }
button:hover,.raw:hover { border-color:var(--accent); color:var(--accent); }
main { width:min(100% - 2rem,1280px); margin:clamp(1rem,4vw,3rem) auto; overflow:hidden; border:1px solid var(--line); border-radius:.85rem; background:var(--surface); box-shadow:0 22px 60px rgba(54,59,92,.08); }
pre { overflow:auto; min-height:calc(100vh - 10rem); margin:0; padding:1.1rem 0 2rem; counter-reset:line; tab-size:2; }
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
@media (max-width:700px) { header { padding-inline:.65rem; } .meta { display:none; } header button { padding-inline:.5rem; } main { width:100%; margin:0; border-width:0; border-radius:0; box-shadow:none; } pre { min-height:calc(100vh - 3.6rem); } .line::before { width:3.4rem; margin-right:.8rem; } body.wrap .line { padding-left:4.2rem; } body.wrap .line::before { margin-right:0; } }
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
:root { color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#272a38; --muted:#73788b; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --grid:#dfe3ec; }
* { box-sizing:border-box; }
html,body { width:100%; min-height:100%; }
body { margin:0; color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC",sans-serif; }
header { position:sticky; top:0; z-index:2; display:flex; align-items:center; gap:.65rem; min-height:3.6rem; padding:.7rem max(1rem,calc((100vw - 1440px)/2)); border-bottom:1px solid var(--line); background:color-mix(in srgb,var(--paper) 90%,transparent); backdrop-filter:blur(16px); }
.back { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.5rem; color:var(--muted); text-decoration:none; }
.back:hover { color:var(--accent); background:var(--accent-soft); }
.kind { flex:0 0 auto; padding:.28rem .5rem; border-radius:.35rem; color:white; background:var(--accent); font:750 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.05em; }
.filename { overflow:hidden; font:650 .82rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; }
.meta { margin-left:auto; color:var(--muted); font:500 .7rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; white-space:nowrap; }
button,.raw { min-height:2rem; padding:.45rem .65rem; border:1px solid var(--line); border-radius:.45rem; color:var(--ink); background:var(--surface); font:650 .72rem/1 ui-sans-serif,-apple-system,sans-serif; text-decoration:none; cursor:pointer; }
button:hover,.raw:hover { border-color:var(--accent); color:var(--accent); }
.canvas { display:grid; place-items:center; width:min(100% - 2rem,1440px); height:calc(100vh - 5.6rem); height:calc(100dvh - 5.6rem); min-height:18rem; margin:1rem auto; overflow:auto; border:1px solid var(--line); border-radius:.9rem; background-color:var(--surface); background-image:linear-gradient(45deg,var(--grid) 25%,transparent 25%),linear-gradient(-45deg,var(--grid) 25%,transparent 25%),linear-gradient(45deg,transparent 75%,var(--grid) 75%),linear-gradient(-45deg,transparent 75%,var(--grid) 75%); background-position:0 0,0 8px,8px -8px,-8px 0; background-size:16px 16px; box-shadow:0 22px 60px rgba(54,59,92,.08); }
#artwork { display:block; max-width:calc(100% - 3rem); max-height:calc(100% - 3rem); }
body.actual .canvas { place-items:start; padding:1.5rem; }
body.actual #artwork { max-width:none; max-height:none; }
#error { align-self:center; justify-self:center; padding:1rem 1.25rem; border:1px solid var(--line); border-radius:.65rem; color:var(--muted); background:var(--surface); }
:focus-visible { outline:3px solid color-mix(in srgb,var(--accent) 55%,transparent); outline-offset:2px; }
@media (max-width:700px) { header { padding-inline:.65rem; } .meta { display:none; } .canvas { width:100%; height:calc(100vh - 3.6rem); height:calc(100dvh - 3.6rem); margin:0; border-width:0; border-radius:0; } #artwork { max-width:calc(100% - 2rem); max-height:calc(100% - 2rem); } }
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
:root { color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#272a38; --muted:#73788b; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --grid:#e7e9f1; }
* { box-sizing:border-box; }
html,body { width:100%; min-height:100%; }
body { margin:0; color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC",sans-serif; }
header { position:sticky; top:0; z-index:3; display:flex; align-items:center; gap:.65rem; min-height:3.6rem; padding:.7rem max(1rem,calc((100vw - 1500px)/2)); border-bottom:1px solid var(--line); background:color-mix(in srgb,var(--paper) 90%,transparent); backdrop-filter:blur(16px); }
.back { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.5rem; color:var(--muted); text-decoration:none; }
.back:hover { color:var(--accent); background:var(--accent-soft); }
.kind { flex:0 0 auto; padding:.28rem .5rem; border-radius:.35rem; color:white; background:var(--accent); font:750 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.05em; }
.filename { overflow:hidden; font:650 .82rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; }
.meta { margin-left:auto; color:var(--muted); font:500 .7rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; white-space:nowrap; }
.raw { min-height:2rem; padding:.55rem .65rem; border:1px solid var(--line); border-radius:.45rem; color:var(--ink); background:var(--surface); font:650 .72rem/1 ui-sans-serif,-apple-system,sans-serif; text-decoration:none; }
.raw:hover { border-color:var(--accent); color:var(--accent); }
.canvas { position:relative; width:min(100% - 2rem,1500px); height:calc(100vh - 5.6rem); height:calc(100dvh - 5.6rem); min-height:22rem; margin:1rem auto; overflow:auto; border:1px solid var(--line); border-radius:.9rem; background-color:var(--surface); background-image:linear-gradient(var(--grid) 1px,transparent 1px),linear-gradient(90deg,var(--grid) 1px,transparent 1px); background-size:24px 24px; box-shadow:0 22px 60px rgba(54,59,92,.08); }
.mxgraph { min-width:100%; min-height:100%; padding:2rem; border:1px solid transparent; }
.viewer-status { position:absolute; inset:0; z-index:2; display:grid; place-content:center; justify-items:center; gap:.55rem; color:var(--ink); background:color-mix(in srgb,var(--surface) 88%,transparent); text-align:center; backdrop-filter:blur(4px); }
.viewer-status[hidden] { display:none; }
.viewer-status small { color:var(--muted); font:.65rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.08em; text-transform:uppercase; }
.spinner { width:1.65rem; height:1.65rem; margin-bottom:.25rem; border:2px solid var(--line); border-top-color:var(--accent); border-radius:50%; animation:spin .7s linear infinite; }
.viewer-status.error .spinner { display:none; }
.viewer-status.error strong { color:#bd3e52; }
:focus-visible { outline:3px solid color-mix(in srgb,var(--accent) 55%,transparent); outline-offset:2px; }
@keyframes spin { to { transform:rotate(360deg); } }
@media (max-width:700px) { header { padding-inline:.65rem; } .meta { display:none; } .canvas { width:100%; height:calc(100vh - 3.6rem); height:calc(100dvh - 3.6rem); margin:0; border-width:0; border-radius:0; } .mxgraph { padding:1rem; } }
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

const DIRECTORY_CSS: &str = r#"
:root { color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#202333; --muted:#73788b; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --folder:#6970e8; }
* { box-sizing:border-box; }
html { font-size:16px; }
body { min-height:100vh; margin:0; color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC","PingFang SC",sans-serif; }
main { width:min(100% - 2rem,980px); margin:0 auto; padding:clamp(1.5rem,6vw,5rem) 0; }
.breadcrumbs { display:flex; align-items:center; gap:.55rem; overflow-x:auto; padding-bottom:1rem; color:var(--muted); font:600 .78rem/1.4 ui-monospace,SFMono-Regular,Consolas,monospace; scrollbar-width:none; }
.breadcrumbs a { color:inherit; text-decoration:none; white-space:nowrap; }
.breadcrumbs a:hover { color:var(--accent); }
header { position:relative; padding:clamp(1.4rem,4vw,2.5rem); overflow:hidden; border:1px solid var(--line); border-radius:1.1rem 1.1rem 0 0; background:var(--surface); }
header::after { position:absolute; right:-1.4rem; bottom:-3.2rem; width:9rem; height:7rem; border:1.1rem solid var(--accent-soft); border-radius:1.2rem; content:""; transform:rotate(-8deg); }
.eyebrow { margin:0 0 .7rem; color:var(--accent); font:700 .72rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.14em; }
h1 { position:relative; z-index:1; margin:0; overflow-wrap:anywhere; font-family:"Iowan Old Style","Noto Serif SC","Songti SC",Georgia,serif; font-size:clamp(2rem,6vw,3.8rem); line-height:1.08; letter-spacing:-.035em; }
.summary { position:relative; z-index:1; margin:.8rem 0 0; color:var(--muted); font-size:.88rem; }
.listing { overflow:hidden; border:1px solid var(--line); border-top:0; border-radius:0 0 1.1rem 1.1rem; background:var(--surface); box-shadow:0 25px 70px rgba(54,59,92,.09); }
.entry { display:grid; grid-template-columns:2rem minmax(0,1fr) 5rem 6rem 1.5rem; align-items:center; gap:.8rem; min-height:4.15rem; padding:.65rem 1.2rem; border-top:1px solid var(--line); color:var(--ink); text-decoration:none; transition:background .15s ease,padding-left .15s ease; }
.entry:first-child { border-top:0; }
.entry:hover { padding-left:1.45rem; background:var(--accent-soft); }
.entry-name { overflow:hidden; font-weight:650; text-overflow:ellipsis; white-space:nowrap; }
.kind { justify-self:start; padding:.2rem .45rem; border:1px solid var(--line); border-radius:.3rem; color:var(--muted); font:700 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.04em; }
.detail { justify-self:end; color:var(--muted); font:500 .75rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; }
.arrow { color:var(--muted); font-size:1.1rem; opacity:0; transform:translateX(-.35rem); transition:opacity .15s ease,transform .15s ease; }
.entry:hover .arrow { opacity:1; transform:none; }
.glyph { position:relative; display:block; width:1.4rem; height:1.2rem; border:2px solid var(--muted); border-radius:.18rem; opacity:.75; }
.folder .glyph { height:1rem; margin-top:.2rem; border:0; border-radius:.18rem; background:var(--folder); opacity:1; }
.folder .glyph::before { position:absolute; left:.08rem; top:-.28rem; width:.62rem; height:.38rem; border-radius:.18rem .18rem 0 0; background:var(--folder); content:""; }
.file .glyph::after { position:absolute; right:-2px; top:-2px; width:.42rem; height:.42rem; border-left:2px solid var(--muted); border-bottom:2px solid var(--muted); background:var(--surface); content:""; }
.empty { display:grid; place-items:center; min-height:14rem; color:var(--muted); }
.empty span { font:300 3rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; }
.empty p { margin:.8rem 0 0; }
:focus-visible { outline:3px solid color-mix(in srgb,var(--accent) 55%,transparent); outline-offset:-3px; }
@media (max-width:650px) { main { width:100%; padding:1rem; } header { padding:1.5rem 1.1rem; } .entry { grid-template-columns:1.8rem minmax(0,1fr) auto; padding-inline:1rem; } .kind,.arrow { display:none; } .detail { grid-column:3; } }
@media (prefers-color-scheme:dark) { :root { --paper:#11131b; --surface:#191c27; --ink:#edf0f7; --muted:#a7adbd; --line:#303545; --accent:#a9a5ff; --accent-soft:#292943; --folder:#8e8af5; } body { background-image:radial-gradient(circle at 50% -20%,#252943 0,transparent 38rem); } .listing { box-shadow:0 25px 70px rgba(0,0,0,.25); } }
@media (prefers-reduced-motion:reduce) { .entry,.arrow { transition:none; } }
"#;

const MARKDOWN_CSS: &str = r#"
:root { color-scheme:light dark; --paper:#f7f8fc; --surface:#fff; --ink:#202333; --muted:#6d7287; --line:#dfe3ee; --accent:#5b5bd6; --accent-soft:#eeeeff; --code:#171925; --code-ink:#e8eaf2; --quote:#eef4ff; }
* { box-sizing:border-box; }
[hidden] { display:none !important; }
html { font-size:17px; scroll-padding-top:5rem; }
body { margin:0; color:var(--ink); background:var(--paper); font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC","PingFang SC",sans-serif; line-height:1.78; }
.topbar { position:sticky; top:0; z-index:4; display:flex; align-items:center; gap:.75rem; min-height:3.4rem; padding:.7rem max(1rem,calc((100vw - 1120px)/2)); border-bottom:1px solid color-mix(in srgb,var(--line) 75%,transparent); background:color-mix(in srgb,var(--paper) 88%,transparent); backdrop-filter:blur(16px); }
.home { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.5rem; color:var(--muted); text-decoration:none; }
.home:hover { color:var(--accent); background:var(--accent-soft); }
.mark { display:grid; place-items:center; width:2rem; height:2rem; border-radius:.55rem; color:white; background:var(--accent); font:700 .68rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:-.04em; transform:rotate(-3deg); }
.filename { overflow:hidden; color:var(--muted); font:600 .8rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace; text-overflow:ellipsis; white-space:nowrap; }
.top-actions { display:flex; gap:.55rem; margin-left:auto; }
.button,.format-tools button { border:1px solid var(--line); border-radius:.5rem; color:white; background:var(--accent); font:650 .78rem/1 ui-sans-serif,-apple-system,sans-serif; cursor:pointer; }
.button { display:inline-grid; place-items:center; min-height:2rem; padding:.5rem .8rem; text-decoration:none; }
.button.ghost { color:var(--ink); background:var(--surface); }
.button.danger { border-color:#bd3e52; background:#bd3e52; }
.button:hover,.format-tools button:hover { filter:brightness(.96); transform:translateY(-1px); }
.reader-layout { display:grid; grid-template-columns:180px minmax(0,920px); gap:2rem; justify-content:center; width:min(100% - 2rem,1140px); margin:clamp(1.25rem,5vw,4rem) auto; }
main.paper { width:100%; margin:0; padding:clamp(1.25rem,5vw,4.6rem); border:1px solid var(--line); border-radius:1.1rem; background:var(--surface); box-shadow:0 24px 70px rgba(54,59,92,.09); }
#toc { position:sticky; top:5rem; align-self:start; max-height:calc(100vh - 7rem); overflow:auto; padding:.35rem; font-size:.74rem; }
#toc:empty { display:none; }
#toc::before { display:block; margin:0 0 .7rem .55rem; color:var(--muted); content:"ON THIS PAGE"; font:700 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.1em; }
#toc a { display:block; overflow:hidden; padding:.35rem .55rem; border-left:1px solid var(--line); color:var(--muted); text-decoration:none; text-overflow:ellipsis; white-space:nowrap; }
#toc a[data-level="3"] { padding-left:1.15rem; }
#toc a:hover { border-color:var(--accent); color:var(--accent); }
article { max-width:760px; margin:auto; }
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
.editor-shell { display:flex; flex-direction:column; height:calc(100vh - 3.4rem); height:calc(100dvh - 3.4rem); overflow:hidden; background:var(--surface); }
.editor-toolbar { display:flex; flex:0 0 auto; align-items:center; gap:.6rem; min-height:3.5rem; padding:.6rem max(1rem,calc((100vw - 1400px)/2)); border-bottom:1px solid var(--line); }
.format-tools { display:flex; gap:.3rem; overflow-x:auto; }
.format-tools button { flex:0 0 auto; width:2rem; height:2rem; padding:0; color:var(--ink); background:var(--paper); }
#save-status { margin-left:auto; color:var(--muted); font-size:.75rem; }
.editor-panes { display:grid; flex:1 1 auto; grid-template-columns:minmax(0,1fr) minmax(0,1fr); min-height:0; }
.pane { display:grid; grid-template-rows:2rem 1fr; min-width:0; min-height:0; margin:0; }
.pane > span { padding:.7rem 1rem; color:var(--muted); background:var(--paper); font:700 .62rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; letter-spacing:.12em; }
.preview-pane { border-right:1px solid var(--line); }
#source { width:100%; height:100%; resize:none; padding:1.25rem clamp(1rem,3vw,2rem); border:0; outline:0; color:var(--ink); background:var(--surface); font:500 .9rem/1.75 ui-monospace,SFMono-Regular,Consolas,"Noto Sans Mono CJK SC",monospace; tab-size:2; }
#preview { width:100%; height:100%; border:0; background:var(--paper); }
.preview-body { min-height:100vh; }
main.preview-paper { width:100%; margin:0; padding:2rem; border:0; border-radius:0; box-shadow:none; }
body.editing { overflow:hidden; }
.confirm-dialog { width:min(calc(100% - 2rem),26rem); padding:1.6rem; border:1px solid var(--line); border-radius:1rem; color:var(--ink); background:var(--surface); box-shadow:0 28px 90px rgba(24,27,42,.3); }
.confirm-dialog::backdrop { background:rgba(17,19,27,.52); backdrop-filter:blur(5px); }
.confirm-dialog[open] { animation:dialog-in .16s ease-out both; }
.dialog-mark { display:grid; place-items:center; width:2.25rem; height:2.25rem; margin-bottom:1.1rem; border-radius:.65rem; color:#bd3e52; background:color-mix(in srgb,#bd3e52 13%,var(--surface)); font:800 1.05rem/1 ui-monospace,SFMono-Regular,Consolas,monospace; }
.confirm-dialog h2 { margin:0 0 .55rem; padding:0; border:0; font:700 1.35rem/1.3 ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC",sans-serif; letter-spacing:-.015em; }
.confirm-dialog p { margin:0; color:var(--muted); font-size:.86rem; line-height:1.65; }
.dialog-actions { display:flex; justify-content:flex-end; gap:.55rem; margin-top:1.5rem; }
@keyframes dialog-in { from { opacity:0; transform:translateY(8px) scale(.98); } }
@media (max-width:900px) { .reader-layout { display:block; } #toc { display:none; } }
@media (max-width:700px) { html { font-size:16px; } .reader-layout { display:block; width:100%; margin:0; } main.paper { padding:1.5rem 1rem 3rem; border-width:0; border-radius:0; box-shadow:none; } .topbar { padding-inline:.7rem; } .mark { display:none; } .editor-panes { grid-template-columns:1fr; grid-template-rows:1fr 1fr; } .preview-pane { border-right:0; border-bottom:1px solid var(--line); } #save-status { display:none; } }
@media (prefers-color-scheme:dark) { :root { --paper:#11131b; --surface:#191c27; --ink:#edf0f7; --muted:#a7adbd; --line:#303545; --accent:#a9a5ff; --accent-soft:#292943; --code:#0d0f16; --code-ink:#e7e9f3; --quote:#20283a; } body { background-image:radial-gradient(circle at 50% -20%,#252943 0,transparent 38rem); } code { color:#f2a7ca; } main { box-shadow:0 24px 70px rgba(0,0,0,.25); } }
@media (prefers-reduced-motion:reduce) { .confirm-dialog[open] { animation:none; } }
@media (prefers-reduced-motion:no-preference) { main.paper { animation:arrive .35s ease-out both; } @keyframes arrive { from { opacity:0; transform:translateY(8px); } } }
"#;

const MARKDOWN_JS: &str = r#"
const reader = document.querySelector('#reader');
const editor = document.querySelector('#editor');
const source = document.querySelector('#source');
const preview = document.querySelector('#preview');
const status = document.querySelector('#save-status');
const discardDialog = document.querySelector('#discard-dialog');
let original = source.value;
let previewTimer;
let scrollSyncLocked = false;

function scrollRatio(scrollTop, scrollHeight, clientHeight) {
  const available = scrollHeight - clientHeight;
  return available > 0 ? scrollTop / available : 0;
}

function withScrollLock(callback) {
  if (scrollSyncLocked) return;
  scrollSyncLocked = true;
  callback();
  requestAnimationFrame(() => { scrollSyncLocked = false; });
}

function syncPreviewFromSource() {
  const win = preview.contentWindow;
  const doc = preview.contentDocument;
  if (!win || !doc) return;
  const ratio = scrollRatio(source.scrollTop, source.scrollHeight, source.clientHeight);
  const previewHeight = Math.max(doc.documentElement.scrollHeight, doc.body.scrollHeight);
  withScrollLock(() => win.scrollTo(0, ratio * Math.max(0, previewHeight - win.innerHeight)));
}

function syncSourceFromPreview() {
  const win = preview.contentWindow;
  const doc = preview.contentDocument;
  if (!win || !doc) return;
  const previewHeight = Math.max(doc.documentElement.scrollHeight, doc.body.scrollHeight);
  const ratio = scrollRatio(win.scrollY, previewHeight, win.innerHeight);
  withScrollLock(() => {
    source.scrollTop = ratio * Math.max(0, source.scrollHeight - source.clientHeight);
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
  status.textContent = '正在生成预览…';
  try {
    const response = await fetch(`${location.pathname}?mode=preview`, {
      method: 'POST',
      headers: {'Content-Type': 'text/plain; charset=utf-8'},
      body: source.value
    });
    if (!response.ok) throw new Error(await response.text());
    preview.srcdoc = await response.text();
    status.textContent = source.value === original ? '已保存' : '有未保存修改';
  } catch (error) {
    status.textContent = `预览失败：${error.message}`;
  }
}

function schedulePreview() {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(updatePreview, 220);
  status.textContent = source.value === original ? '已保存' : '有未保存修改';
}

function openEditor() {
  reader.hidden = true;
  editor.hidden = false;
  document.body.classList.add('editing');
  source.focus();
  updatePreview();
}

function leaveEditor(discardChanges) {
  if (discardChanges) source.value = original;
  reader.hidden = false;
  editor.hidden = true;
  document.body.classList.remove('editing');
}

function closeEditor() {
  if (source.value !== original) {
    discardDialog.showModal();
    return;
  }
  leaveEditor(false);
}

function wrapSelection(prefix, suffix = prefix, placeholder = '文本') {
  const start = source.selectionStart;
  const end = source.selectionEnd;
  const selected = source.value.slice(start, end) || placeholder;
  source.setRangeText(`${prefix}${selected}${suffix}`, start, end, 'select');
  source.selectionStart = start + prefix.length;
  source.selectionEnd = start + prefix.length + selected.length;
  source.focus();
  schedulePreview();
}

function prefixLines(prefix) {
  const start = source.value.lastIndexOf('\n', Math.max(0, source.selectionStart - 1)) + 1;
  const nextBreak = source.value.indexOf('\n', source.selectionEnd);
  const end = nextBreak < 0 ? source.value.length : nextBreak;
  const text = source.value.slice(start, end).split('\n').map(line => `${prefix}${line}`).join('\n');
  source.setRangeText(text, start, end, 'select');
  source.focus();
  schedulePreview();
}

document.querySelector('#edit-button').addEventListener('click', openEditor);
document.querySelector('#cancel-button').addEventListener('click', closeEditor);
document.querySelector('#keep-editing-button').addEventListener('click', () => {
  discardDialog.close();
  source.focus();
});
document.querySelector('#discard-button').addEventListener('click', () => {
  discardDialog.close();
  leaveEditor(true);
});
discardDialog.addEventListener('cancel', () => setTimeout(() => source.focus()));
discardDialog.addEventListener('click', (event) => {
  if (event.target === discardDialog) {
    discardDialog.close();
    source.focus();
  }
});
document.querySelector('.format-tools').addEventListener('click', (event) => {
  const action = event.target.closest('button')?.dataset.format;
  if (!action) return;
  if (action === 'heading') prefixLines('## ');
  if (action === 'bold') wrapSelection('**');
  if (action === 'italic') wrapSelection('_');
  if (action === 'link') wrapSelection('[', '](https://)', '链接文字');
  if (action === 'quote') prefixLines('> ');
  if (action === 'code') wrapSelection('`');
  if (action === 'list') prefixLines('- ');
  if (action === 'task') prefixLines('- [ ] ');
});
source.addEventListener('input', schedulePreview);
source.addEventListener('scroll', syncPreviewFromSource, {passive: true});
preview.addEventListener('load', () => {
  preview.contentWindow?.addEventListener('scroll', syncSourceFromPreview, {passive: true});
  syncPreviewFromSource();
});
source.addEventListener('keydown', (event) => {
  if (event.key === 'Tab') {
    event.preventDefault();
    source.setRangeText('  ', source.selectionStart, source.selectionEnd, 'end');
    schedulePreview();
  }
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 's') {
    event.preventDefault();
    document.querySelector('#save-button').click();
  }
});
document.querySelector('#save-button').addEventListener('click', async () => {
  status.textContent = '正在保存…';
  try {
    const response = await fetch(`${location.pathname}?mode=raw`, {
      method: 'PUT',
      headers: {'Content-Type': 'text/markdown; charset=utf-8'},
      body: source.value
    });
    if (!response.ok) throw new Error(await response.text());
    original = source.value;
    status.textContent = '已保存';
    setTimeout(() => location.reload(), 250);
  } catch (error) {
    status.textContent = `保存失败：${error.message}`;
  }
});
window.addEventListener('beforeunload', (event) => {
  if (source.value !== original) { event.preventDefault(); event.returnValue = ''; }
});
buildViewerTools();
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

    #[test]
    fn parses_default_and_custom_ports() {
        assert_eq!(
            parse_args(Vec::<String>::new().into_iter()).unwrap(),
            Some(PortConfig {
                port: 8080,
                fallback_to_random: true,
            })
        );
        assert_eq!(
            parse_args(vec!["-p".into(), "3000".into()].into_iter()).unwrap(),
            Some(PortConfig {
                port: 3000,
                fallback_to_random: false,
            })
        );
    }

    #[test]
    fn falls_back_to_a_random_port_when_default_is_in_use() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let listener = bind_listener(&PortConfig {
            port: occupied_port,
            fallback_to_random: true,
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
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
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
        assert!(page.contains("<h1>Hello</h1>"));
        assert!(page.contains("<table>"));
        assert!(page.contains("type=\"checkbox\""));
        assert!(page.contains("README.md"));
        assert!(page.contains("id=\"edit-button\""));
        assert!(page.contains("id=\"source\""));
        assert!(page.contains("[hidden] { display:none !important; }"));
        assert!(page.contains(".editor-shell { display:flex; flex-direction:column;"));
        assert!(
            page.find("class=\"pane preview-pane\"").unwrap()
                < page.find("class=\"pane source-pane\"").unwrap()
        );
        assert!(page.contains(".preview-pane { border-right:1px solid var(--line); }"));
        assert!(page.contains("source.addEventListener('scroll', syncPreviewFromSource"));
        assert!(page.contains("addEventListener('scroll', syncSourceFromPreview"));
        assert!(page.contains("<dialog id=\"discard-dialog\""));
        assert!(page.contains("id=\"keep-editing-button\""));
        assert!(page.contains("id=\"discard-button\""));
        assert!(!page.contains("confirm('"));
        assert!(page.contains("href=\"/favicon.svg\""));
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
    fn renders_svg_in_an_isolated_image_preview() {
        let page = render_svg_page("icon<&>.svg", 1536);

        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("src=\"?mode=asset\""));
        assert!(page.contains("href=\"?mode=raw\""));
        assert!(page.contains("class=\"canvas\""));
        assert!(page.contains("icon&lt;&amp;&gt;.svg"));
        assert!(!page.contains("icon<&>.svg"));
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
