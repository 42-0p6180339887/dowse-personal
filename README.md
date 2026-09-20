# Dowse Personal · 本地文档全文搜索

基于 [ltspace/dowse](https://github.com/ltspace/dowse) 的 Windows 个人文档搜索应用。采用搜索框、结果表格与固定预览的桌面流程，便于连续查找和阅读资料；不依赖 AnyTXT，也不使用其代码或资源。

这是 **2026-09-21 的可运行版本存档**。保留上游 Git 历史及 MIT / Apache-2.0 许可证，个人版改造基线为 `4026a34562bb0f79ea9e44d5d0b6c060de300baa`。该仓库不是上游官方发行版。

## 下载和使用

到 [Releases](https://github.com/42-0p6180339887/dowse-personal/releases) 下载 Windows x64 压缩包，解压后运行 `dowse-app.exe`。需要 Windows WebView2 Runtime；无需启动开发服务器。当前交付是直接运行 EXE，未提供安装器或代码签名。

1. 添加要搜索的资料文件夹，等待建立索引。建议从具体文档目录开始，不扫描整盘。
2. 输入关键词，按文件类型、目录、内容/文件名模式筛选，支持排序与分页。
3. 单击结果预览，双击或 Enter 打开原文件；切换“全文阅读”可加载正文和文内查找。
4. 选定目录中的文件新增、修改、删除后自动更新，重启保留索引。

个人版数据位于 `%LOCALAPPDATA%\dowse-personal\data`，与上游默认数据目录隔离。默认不开机自启，窗口失焦后保持可见。

## 功能与边界

| 功能 | 当前版本 |
| --- | --- |
| 正文提取 | 文字 PDF、DOCX、XLSX、PPTX、TXT、MD |
| Excel | 工作表顺序、表名、单元格位置、共享/内联文字、数字、布尔值、已有公式缓存 |
| PowerPoint | 按 presentation 关系定义的实际页序提取，保留页序标记 |
| 阅读与查找 | 索引正文分块读取；字面查找、忽略英文大小写；中文、emoji、跨块命中跳转 |
| 结果交互 | 可调整列宽、固定预览、类型/范围/模式筛选、排序、分页 |
| 更新一致性 | 索引提交合并刷新；正文缓存随索引快照失效；丢弃过期界面响应 |
| 错误反馈 | 不支持、损坏、加密、无可提取文字、超过限制等状态 |

限制：旧版 `.doc/.xls/.ppt`、扫描 PDF 和图片 OCR 未纳入个人版交付；预览为提取文字，不还原原排版；公式不重新计算。大型真实资料库的内存、首次建库耗时和长期监听尚未完成系统压测。Office 临时锁文件暂未排除。

正文已使用上游 LZ4 压缩。本版增加有界缓存和刷新调度，没有删除检索字段或截断全文，也没有足够的同语料前后对照来宣称索引缩小或算法加速比例。

## 从源码构建

环境：Windows x64、近期 Rust stable（MSVC 工具链）、Node.js 22.12+、Visual Studio 2022 C++ Build Tools 与 Windows SDK。请在 **Developer PowerShell for VS 2022** 中执行：

```powershell
.\Build-Windows.ps1
.\target\release\dowse-app.exe
```

脚本按顺序安装前端依赖并构建、生成 CLI sidecar、构建独立桌面应用。桌面构建必须启用 `--features custom-protocol`，否则可能仍连接开发地址并出现 localhost 拒绝连接。重建前先关闭运行中的 EXE。

前端与 Rust 验证入口：

```powershell
npm --prefix crates/dowse-app run check
npm --prefix crates/dowse-app run test:unit
cargo test -p dowse
```

用于构造合成资料的标准库脚本：`python scripts/generate-test-documents.py <独立输出目录>`。仓库不包含个人文件、索引、配置、模型凭据或本机过程日志。上游工作流在此存档中只允许手动触发，不自动部署官网或生成其他发行包。

## 验收与来源

参见 [验收摘要](docs/ACCEPTANCE.md)：记录实际运行的功能、具体构建标识和未测范围。编译成功不等于全部功能通过。

本仓库沿用上游的 **MIT OR Apache-2.0** 双许可，原版权声明保留于 [LICENSE-MIT](LICENSE-MIT) 和 [LICENSE-APACHE](LICENSE-APACHE)。上游的其他语言说明、开发文档及网站源码作为来源历史保留，可能描述本个人版未验收的功能；个人版能力以本 README 为准。

English: A Windows document full-text search and reading app derived from Dowse, with a persistent desktop window, tabular results, indexed full-text preview, literal in-document search, and improved Office extraction. This repository archives the tested personal edition; see the limitations and acceptance summary above.
