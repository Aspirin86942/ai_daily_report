# 日报 {{ report.date }}

生成时间：{{ report.generated_at | shanghai_time }}

{% if report.is_historical_reconstruction %}**历史补录。** 本报告按文件最后修改时间归档，摘要依据补录时仍可读取的内容。已被覆盖的旧版本无法还原；没有发现来源也不能据此认定当天没有工作。

{% endif %}
扫描范围：{{ report.root_alias }}；时间窗：{{ report.window_start_utc | shanghai_time }} 至 {{ report.window_end_utc | shanghai_time }}（右端不包含）。

**质量：{{ report.quality_label }}。** 来源 {{ report.sources | length }} 份。

以下内容根据实际读取时的文件内容与用户陈述整理。文档中提及的安排、进度和结果，保留其来源语义。

## 文件内容摘要
{% for item in report.summary_items %}
- {{ item.text | evidence_text }} {% for source_id in item.source_ids %}[{{ report.source_labels[source_id] }}]{% if not loop.last %}、{% endif %}{% endfor %}
{% else %}
{% if report.quality == 'no_changes' %}
在目标时间窗内未发现符合扫描规则的修改文件。
{% else %}
现有材料没有产生可归纳的摘要条目。
{% endif %}
{% endfor %}
## 待核实事项
{% for item in report.verification_items %}
- {{ item.text | evidence_text }} {% for source_id in item.source_ids %}[{{ report.source_labels[source_id] }}]{% if not loop.last %}、{% endif %}{% endfor %}
{% else %}
未提取到额外的待核实事项。
{% endfor %}
## 来源与覆盖
{% for source in report.sources %}
### {{ report.source_labels[source.source_id] }} · {{ (source.relative_path or '用户主动补充的陈述') | evidence_text }}

- 来源类型：{{ '文件内容' if source.source_kind == 'file' else '用户陈述，未经独立核实' }}。
{% if source.modified_at_utc %}- 修改时间：{{ source.modified_at_utc | shanghai_time }}。
{% endif %}- 观察时间：{{ source.observed_at_utc | shanghai_time }}。
- 提取 {{ source.excerpt_chars }} 个字符；送入模型 {{ source.model_chars }} 个字符。
- 提取范围：{{ '解析有截断' if source.parse_truncated else '解析未报告截断' }}；{{ '摘录有截断' if source.excerpt_truncated else '摘录未截断' }}；{{ '模型输入有遗漏' if source.model_truncated else '模型输入未报告遗漏' }}。
{% if source.extension == '.csv' %}- CSV 覆盖：最多 3 行样本，不代表读取完整表格。
{% endif %}{% if source.coverage_limits.get('pdf') and source.extension == '.pdf' %}- PDF 页数上限：{{ source.coverage_limits.pdf.max_pages }}。
{% endif %}{% if source.coverage_limits.get('office') and source.extension in ['.xlsx', '.xls'] %}- 表格覆盖上限：{{ source.coverage_limits.office.excel_max_sheets }} 个工作表，每表 {{ source.coverage_limits.office.excel_max_rows }} 行、{{ source.coverage_limits.office.excel_max_columns }} 列。
{% endif %}{% if source.content_sha256 %}- 原文件 SHA-256 校验值：`{{ source.content_sha256 }}`。
{% endif %}{% else %}
本日期没有符合条件的来源。
{% endfor %}
## 缺漏与异常
{% for gap in report.gaps %}
- {{ gap.message | evidence_text }}{% if gap.relative_path %} 范围：{{ gap.relative_path | evidence_text }}。{% endif %}{% if gap.source_id %} [{{ report.source_labels[gap.source_id] }}]{% endif %}
{% else %}
未记录到影响本报告的采集缺漏。
{% endfor %}{% if report.missed_dates %}
未自动补写的历史日期：{{ report.missed_dates | join('、') }}。
{% endif %}
{% if report.user_notes %}## 用户附注

{{ report.user_notes }}
{% endif %}
