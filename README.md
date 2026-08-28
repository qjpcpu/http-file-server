# http

一个使用 Rust 标准库实现的简单静态网站服务器。

访问任意目录路径时始终显示该目录下的目录和文件列表，可以逐级进入目录或打开文件。
HTML 文件会以网页形式直接渲染；使用 `?mode=raw` 可以查看其源码。
Markdown 文件会渲染为带目录和代码复制能力的阅读页面，
也可以切换到分栏编辑器实时预览并保存。任意文件 URL 加上 `?mode=raw` 后会跳过
Markdown 等内容转换，直接返回原始内容。

TOML、XML、YAML、TXT、配置文件、Shell 配置、Dockerfile，以及其他经内容检测确认的
UTF-8 文本文件，会使用带行号、复制和自动换行的只读文本页面展示。二进制魔数、常见
二进制扩展名、NUL、非法 UTF-8 或异常控制字符都会阻止文本渲染。
Rust、SQL、Go、JavaScript/TypeScript、Shell、Lisp、Python、Java、C/C++ 等常见源码
以及 JSON / JSONL 会进一步启用服务端语法高亮。
SVG 文件会显示在带透明网格的图片预览页中，可切换适应画布或原始尺寸，并保留 Raw
源码入口；作为网页资源引用时仍按 `image/svg+xml` 原样返回。
Draw.io 文件会使用随二进制打包的 diagrams.net Viewer 渲染，支持缩放、图层、灯箱和
多页浏览；Viewer 运行时不会加载任何公网资源，Raw 入口仍可查看原始 XML。
服务会根据托管根目录名称的首字符自动生成站点图标，同时尊重目录中已有的
`favicon.svg` 或 `favicon.ico`。

```bash
# 托管当前目录，默认监听 8080；如果 8080 被占用则随机选择可用端口
cargo run --release

# 指定端口
cargo run --release -- -p 3000
```

编译后也可以直接运行：

```bash
cargo build --release
./target/release/http -p 3000
```
