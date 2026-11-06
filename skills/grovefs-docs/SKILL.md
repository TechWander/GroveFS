---
name: grovefs-docs
description: Keeps GroveFS documentation (README.md, USAGE.md, docs.html) stating only current-version behavior — no migration history, no removed flags/env vars, no before/after baselines, no "newly added / now supports / identical to the old / falls back to old behavior" change narration. Use when editing, rewriting, or reviewing these three files, when auditing whether docs match `grovefs --help`, or when asked to "按最新版本写文档" / "排查文档里的历史叙述" / "参数对账".
---

# GroveFS 文档口径（只写当前版本）

三份文档同一规矩：**手册只陈述当前版本的行为，演变史归 git 提交记录。**

- `README.md` — 双语简介 + 「诚实标注的边界」清单（中文末段）
- `USAGE.md` — 使用说明，§14.4 末尾写有口径原文
- `docs.html` — 单文件手册，§21 末尾写有口径原文（两条 `.ok` div）

口径原文本身在三份文档里各写了一次，改口径要**三处同步**。

## 禁止写进文档

1. **已移除的参数 / 环境变量 / 开关名**——连「`--strict-view` 与 `GROVEFS_ALLOW_LAZY_VIEW` 已从代码中移除」这种提法也不行：提及名字等于给旧行为招魂，读者会去搜、去试。
2. **版本前后对比**——「改造前基线 55.34s」「从 55s 降到 2.5s」「N=1 行为与旧版完全一致」「无账本行的历史库退回单视图旧行为」。要表达性能，只写**当前实测值 + 测量条件**。
3. **变更动词**——新增了 / 现已 / 已落地 / 已覆盖 / 现已支持 / gained / now also / fall back to the old。改成陈述句：「X 覆盖 Y」「X 是 Z 的唯一路径」。
4. **一次性迁移指引**——升级后要重挂 / 重铸 key 之类的临时动作。写成当前版本的要求（「定义替换 ≤2s 热生效」），不写「以前要怎么改」。

## 必须保留（别误删）

- **当前版本内的真实回退语义**：跨设备 import 退化为独立拷贝、对接不含该能力位的老服务端时退回全量列表、跨节点 link 拒绝并给出原因。这是行为契约，不是历史。
- **「未实现（诚实标注）」负空间清单**：`--subpath`/`--prefix` 等明确说没有的能力，是文档职责。
- **真实程序输出引文**（`no longer resolves`、demo 目录名 `/data/legacy`）、**运维回滚动作**（出问题换回原二进制）。
- 无实测数字处不做收益声称；有限制处在「诚实标注」章节明示。

## 工作流程

1. 先扫再改，不要凭印象。按这四族关键词分别定位（终端在 CJK 换行处会插假空格，**别把显示出来的空格抄进锚点**）：

   ```bash
   cd grovefs && grep -nE '已从代码|已移除|不再提供|strict-view|ALLOW_LAZY_VIEW' README.md USAGE.md docs.html
   grep -nE '改造前|旧版|旧行为|退回旧|与旧.*一致' README.md USAGE.md docs.html
   grep -nE '新增了|新增的|现已|已落地|已经落地|now also|gained' README.md USAGE.md docs.html
   grep -nE '[0-9]+(\.[0-9]+)?s? *(→|->|降到)|基线' README.md USAGE.md docs.html   # 历史基线数字
   ```

2. 改前先核实语义，别用印象填空。行为表述以源码为准（例：空账本的含义查 `src/meta.rs` 模块注释，分页能力查 `MetaProvider::supports_paged_readdir()`）。
3. 分批 SearchReplace，**每批后读 diff**——上一轮的遗漏通常藏在 diff 的上下文行里（`<summary>`、小节标题、英文段落最常漏）。
4. 跑收口检查：

   ```bash
   bash skills/grovefs-docs/scripts/doccheck.sh
   ```

   四项：变更史扫描、参数对账（文档 `--xxx` ↔ `grovefs --help` 真实参数差集）、docs.html 标签闭合、新增用字校验（抓自己打错的汉字）。退出码 0 才算收工。

## 硬边界

- 只改文字与格式，**不改行为描述的事实**。拿不准某句是不是当前行为，去读源码或跑命令，不要靠猜补全。
- 不为「读起来顺」删掉限制、拒因、单位、测量条件。
- 不新增实测以外的数字，不给没跑过的命令贴「真实回显」。
