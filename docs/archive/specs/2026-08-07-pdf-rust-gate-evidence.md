# PDF 解析迁移基准门禁证据

> 日期：2026-08-07
>
> 语料：`tests/fixtures/pdf_benchmark/`（6 份合成中英 PDF，ground truth 已知）
>
> 对比：pdfplumber 0.11.10（Python 现状）与 pdf-extract 0.12.0（Rust 候选）

## 结论

**维持 pdfplumber。** Rust 候选的质量和 P50 速度两项条件都未通过，
不进入 PDF→Rust 迁移设计，不修改生产 `document_parser.py`，也不新增 Rust
生产依赖。

## 语料与核验

- 语料包含 3 份含中文/中英混排的案例与 3 份英文案例；每份均为 A4
  单页 PDF，并有同名 UTF-8 `.txt` ground truth。
- 生成器对 CJK 和 ASCII 分别使用 `STSong-Light` 与 Helvetica，按真实字宽
  换行；PDF 开启压缩与 invariant 元数据，可幂等复测。
- 6 页均经 Poppler 渲染为 PNG 并逐页检查：文字清晰，无裁切、重叠、
  黑方块或异常字距。PNG 是临时 QA 产物，检查后已清理。
- `.gitattributes` 将本语料目录的 PDF 标为 binary，避免 Windows Git 的
  `astextplain`/自动行尾转换损坏 xref。
- 字符保真率使用 NFC 后、忽略版面空白的 `SequenceMatcher`；可打印率允许
  所有 `isprintable()` 字符及正常的 CR/LF/TAB，不把标点误判为乱码。

## 汇总结果

汇总由 `scripts/pdf_benchmark/summarize.py` 从两侧原始 JSON 计算；P90 使用
inclusive 线性插值，吞吐为 6 份文件除以逐文件耗时总和。

| 指标 | pdfplumber | pdf-extract |
|---|---:|---:|
| 成功案例 | 6 / 6 | 3 / 6 |
| 平均 gt_ratio | 1.000 | 0.500 |
| 平均 printable_ratio | 1.000 | 0.500 |
| 耗时 P50 / P90 (ms) | 7.027 / 43.041 | 12.444 / 12.962 |
| 总计时 (ms) | 112.084 | 73.646 |
| 吞吐 (files/s) | 53.531 | 81.471 |

pdfplumber 首份案例包含约 77.637ms 的一次性初始化，因此按总耗时计算的
吞吐低于 Rust；冻结的速度门禁使用 P50，以避免该单个冷初始化样本决定结论。
在 P50 口径下 Rust 是 Python 的 1.771 倍耗时，不是至少 2 倍更快。

## 逐案例质量事实

| 案例 | 内容 | pdfplumber gt_ratio | pdf-extract gt_ratio | Rust 退出码/错误 |
|---|---|---:|---:|---|
| case_01 | 中文 | 1.000 | 0.000 | 101 / `unsupported encoding UniGB-UCS2-H` |
| case_02 | 英文 | 1.000 | 1.000 | 0 |
| case_03 | 中文 | 1.000 | 0.000 | 101 / `unsupported encoding UniGB-UCS2-H` |
| case_04 | 英文 | 1.000 | 1.000 | 0 |
| case_05 | 中英混排 | 1.000 | 0.000 | 101 / `unsupported encoding UniGB-UCS2-H` |
| case_06 | 英文 | 1.000 | 1.000 | 0 |

计划草案指定的 pdf-extract 0.6.5 可以编译，但对 6 份语料均以退出码 0
返回空文本。为避免用旧版本制造失败，最终门禁升级并锁定当前 0.12.0；
最新版能正确提取 3 份英文案例，但对所有含中文案例触发上述 panic。
`rust.json` 只保存可移植错误摘要，不包含本机用户目录或 Cargo registry 路径。

## 门禁判定

### 质量条件：未通过

- Rust 平均 gt_ratio 下限：pdfplumber 1.000 × 95% = **0.950**；
  Rust 实测 **0.500**。
- Rust 平均 printable_ratio 下限：**0.980**；Rust 实测 **0.500**。
- 两个子条件均失败，且 3 个中文案例直接崩溃。

### 速度条件：未通过

- Rust P50 上限：pdfplumber 7.027ms × 50% = **3.514ms**。
- Rust P50 实测 **12.444ms**，为 pdfplumber P50 的 **1.771 倍**。

两项大门禁必须同时通过才可迁移；本次两项均失败，因此唯一结论是维持
pdfplumber。

## 可复跑证据

```powershell
uv run python scripts\pdf_benchmark\generate_corpus.py
uv run python scripts\pdf_benchmark\run_python.py
cargo build --release --locked --manifest-path rust\pdf_extract_bench\Cargo.toml
uv run python scripts\pdf_benchmark\run_rust.py
uv run python scripts\pdf_benchmark\summarize.py
```

- 原始结果：`scripts/pdf_benchmark/results/python.json`、`rust.json`。
- 机器汇总：`scripts/pdf_benchmark/results/summary.json`。
- Rust 候选是带独立 `[workspace]` 与 `Cargo.lock` 的评估 crate；未加入或修改
  生产 scanner workspace。
- reportlab 4.5.1 保留在 dev 依赖中，仅用于幂等重建合成语料。
