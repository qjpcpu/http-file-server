# webdir

把一个本地目录，变成可浏览、可阅读、可审阅的轻量工作空间。

`webdir` 是一个单二进制文件服务器。它不要求项目结构，不依赖前端构建环境：在任意目录启动，
即可获得清晰的文件导航、内容预览、Markdown 协作编辑，以及能被 AI 直接读取的审阅记录。

## 从目录到工作流

- **浏览**：目录列表与照片墙均支持即时搜索、滚动定位和连续浏览。
- **审阅图片**：在 Gallery 中边看边评论；同一目录的评论汇总到一个 JSON 文件，完成后可直接交给 AI 统一修图。
- **审阅文档**：Markdown 支持全文评论、划词批注、回复与状态流转，评论文件与正文相邻保存。
- **协作编辑**：Markdown 编辑器通过 WebSocket 与 CRDT 实时同步，多人修改自动收敛并写回原文件。
- **打开专业内容**：源码、SVG、Mermaid 和 Draw.io 均有对应的阅读界面，同时保留 Raw 原文入口。

界面会跟随系统浅色或深色主题，并针对桌面、平板和手机分别调整交互。所有核心资源随二进制打包，
预览页运行时不依赖公网服务。

## 快速开始

```bash
# 托管当前目录，默认监听 8080
cargo run --release

# 指定端口与目录
cargo run --release -- -p 3000 --dir /data/photos

# 启用持久状态与图片衍生缓存
cargo run --release -- -p 3000 --dir /data/photos --cache /data/webdir.cache

# 启动后写入 PID 文件
cargo run --release -- -p 3000 --pid /tmp/webdir.pid
```

也可以先编译，再直接运行单个二进制：

```bash
cargo build --release
./target/release/webdir -p 3000 --dir /data/photos --cache /data/webdir.cache
```

未显式指定端口时，如果 `8080` 已被占用，服务会自动选择一个可用端口。相对路径以启动时的
当前目录为基准。

## 图片评论与 AI 修图

Gallery 图片弹窗提供评论入口，也可以按 `c` 呼出评论区。评论支持添加、编辑和删除；桌面端使用
侧边工作区，手机和平板使用适合软键盘的底部编辑面板。

同一文件夹内的所有图片共享一个 `.gallery-comments.json`：

```json
{
  "version": 1,
  "comments": [
    {
      "id": "comment-id",
      "image": "portrait.jpg",
      "author": "Alice",
      "body": "压低背景高光，让人物更突出",
      "created_at": "2026-09-09T10:00:00.000Z"
    }
  ]
}
```

这个文件不会出现在目录列表中，但保留在图片旁边，AI 可以一次读取整个文件夹的修改意见，
逐张处理并回写图片。

Gallery 同时提供点赞、待删除标记和批量整理。点击 `favourites` 整理按钮会将当前目录中所有已点赞
图片移入 `favourites/`；目标目录已有同名文件时，新移入的图片会按 `filename_2.ext`、
`filename_3.ext` 依次重命名。常用快捷键如下：

| 操作 | 快捷键 |
| --- | --- |
| 上一张 / 下一张 | `←` `→`、`J` `K` |
| 点赞 / 取消点赞 | `f`，手机上双击图片 |
| 打开 / 关闭评论区 | `c` |
| 轮播 | `p` |
| 标记 / 取消待删除 | `d` |
| 直接删除当前图片 | `Ctrl+K` |

## Markdown 审阅

Markdown 阅读页支持多人实时审阅。首次发表评论时输入审阅人名称；之后可以划选正文添加范围批注，
也可以对全文发表评论。评论按三个状态流转：

1. `open`：待处理。
2. `addressed`：AI 或协作者已处理，等待确认。
3. `resolved`：修改已确认。

审阅数据保存在正文旁边的 `<文件名>.review.json`，例如 `spec.md.review.json`。旁车文件不会显示在
目录列表中，AI 可以直接读取、修改正文，并在对应评论中追加处理说明。

![划选 Markdown 正文并添加待处理评论](docs/images/review-open.png)

```text
codex> 处理掉 your-plan.md.review.json 的评论
```

![Codex 修改正文并回复评论](docs/images/review-addressed.png)

![审阅者确认并解决评论](docs/images/review-resolved.png)

范围批注的 `scope` 包含 UTF-16 `start` / `end`、原文片段和上下文；全文评论使用
`{"type":"document"}`。AI 直接修改评论文件后刷新页面即可载入，通过页面提交的多人评论会实时同步。

## 内容预览

- Markdown：目录、代码复制、Mermaid、实时协作编辑与审阅。
- 源码与文本：服务端语法高亮、行号、复制和自动换行。
- SVG：透明网格、适应画布、原始尺寸与 Raw 源码。
- Draw.io：缩放、图层、灯箱和多页浏览。
- HTML：直接作为网页渲染；添加 `?mode=raw` 可查看源码。

二进制魔数、NUL、非法 UTF-8 和异常控制字符会阻止文件被误判为文本。Markdown 协作文档上限为
16 MiB；服务运行期间，请避免绕过 HTTP 接口直接修改正在协作编辑的文件。

在任意文件 URL 后添加 `?mode=raw`，可以跳过内容转换并读取原始文件。

## 状态与缓存

指定 `--cache DIR` 后，运行状态统一写入 `DIR/webdir.sqlite`：

- `favourites` 保存图片点赞状态。
- `deletion_marks` 保存待删除标记。

未指定 `--cache` 时使用进程内 SQLite，退出服务后状态自然消失。图片移动到整理目录时，相关状态会
跟随新路径；图片被删除时，其状态同步清理。

图片衍生缓存位于 `DIR/thumbnails/`：列表使用 128px 缩略图，Gallery 使用 512px 缩略图和
2560px 屏幕预览图。弹窗先显示已就绪的缩略图，再切换到屏幕预览；进入 1:1 或继续放大时才读取原图。
GIF、SVG、超出处理限制或转换失败的图片直接使用原文件。

缓存目录默认最多占用 2 GiB。服务启动后及每 6 小时清理一次，30 天未使用的衍生图优先删除；
超过上限时按最近使用时间清理到约 1.6 GiB。清理只作用于 `thumbnails/`，不会触碰 `webdir.sqlite`。

页面最多并发加载 4 张缩略图，服务端最多同时生成 2 张衍生图。同一路径、尺寸和文件版本的并发请求
会共享一次处理结果，快速滚动时优先处理靠近视口的图片。

## 测试

Rust 单元与接口测试：

```bash
cargo test
```

Gallery 与 Markdown 的浏览器回归测试使用 Playwright，覆盖桌面 Chromium、桌面 WebKit、Android
尺寸 Chromium、iPhone WebKit 和 iPad WebKit：

```bash
npm ci
npm run test:browser:install
npm run test:browser
```

浏览器测试会自动启动真实的 release 服务，并使用 `tests/browser/fixtures/` 中的独立夹具，不会读取或
修改个人图片目录。

## 静态网站模式

```bash
cargo run --release -- --raw -p 3000 --dir ./public
```

`--raw` 只接受 GET 和 HEAD。目录优先返回 `index.html`，不会生成目录页、预览页、编辑接口或美化错误页；
目录中没有 `index.html` 时返回 `403`。
