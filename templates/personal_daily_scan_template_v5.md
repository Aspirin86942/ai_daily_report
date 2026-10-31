{% if evidence_only %}
<a id="personal-evidence"></a>
# {{ date }} 个人日报 · 完整依据

{% if user_notes %}
## 用户补充原文

{{ user_notes | evidence_text }}

{% endif %}
{% else %}
# {{ date }} 个人日报

{% if quality == "no_changes" %}本次扫描未选入修改时间属于目标日的有效文件，也没有用户补充。无法据此判断当天没有工作。
{% elif items | length == 0 %}本次未能形成可用的事项总结。材料与处理缺口见下方说明。
{% endif %}
{% for item in items %}
## {{ item.title | evidence_text }}

{% if item.statement_kind == "suggestion" %}**建议：** {% elif item.statement_kind == "uncertain" %}**待确认：** {% elif item.statement_kind == "user_statement" %}**用户补充：** {% endif %}{{ item.text | evidence_text }}

{% if item.source_ids %}[查看依据]({{ evidence_link }}#item-{{ loop.index }}){% endif %}
{% endfor %}
{% endif %}
## 材料范围

按目标日 00:00–24:00 的修改时间选取文件，依据为次日扫描时的内容。当天中间版本、已删除文件及扫描前再次修改的文件可能缺失；文件原有内容不代表当天新增成果。

已观察 {{ selection.observed_files }} 个文件，检查 {{ selection.inspected_files }} 个，选入 {{ selection.selected_files }} 个。{% if selection.incomplete_inventory %}观察尚不完整，上述数量为已观察范围。{% endif %}
{% if evidence_only %}
{% for reason, count in selection.omissions | dictsort %}
{% if count %}{% if reason == "duplicate" %}重复材料{% elif reason == "out_of_scope" %}范围外{% elif reason == "budget" %}预算未选{% elif reason == "read_failed" %}读取失败{% elif reason == "comparison_unavailable" %}无法比较{% elif reason == "no_usable_text" %}无可提取文字{% else %}未展开{% endif %}：{{ count }}。
{% endif %}{% endfor %}
{% endif %}
{% for gap in gaps_grouped %}
- {{ gap.message | evidence_text }}{% if gap.count > 1 %}（合并 {{ gap.count }} 条记录）{% endif %}
{% endfor %}
{% if not evidence_only %}
{% if quality == "partial" %}
本次材料覆盖有限，详细范围及省略情况见完整依据。
{% endif %}
[完整依据]({% if evidence_link %}{{ evidence_link }}{% else %}#personal-evidence{% endif %})
{% else %}
{% if items %}
## 事项与来源
{% for item in items %}
<a id="item-{{ loop.index }}"></a>
- {{ item.title | evidence_text }}：{% for id in item.source_ids %}{% for source in sources_view %}{% if source.id == id %}[{{ source.label }}](#{{ source.label | lower }}) {% endif %}{% endfor %}{% endfor %}
{% endfor %}
{% endif %}
{% if sources_view %}
## 关键依据
{% for source in sources_view %}
<a id="{{ source.label | lower }}"></a>
### {{ source.label }} · {{ source.path | evidence_text }}

观察时间：{{ source.observed | shanghai_time }}
{% for ref in source.refs or [] %}
{% if ref.side == "before" %}变更前{% elif ref.side == "after" %}变更后{% else %}本次读取{% endif %} · {{ ref.unit_id | evidence_text }}

文件版本：`{{ ref.version_id }}`；解析版本：`{{ ref.extraction_id }}`。{% for range in ref.ranges %}位置：{{ range.unit }} {{ range.start }}–{{ range.end }}{% if range.label %}（{{ range.label | evidence_text }}）{% endif %}。{% endfor %}

{{ ref.quote | evidence_text }}

内容指纹：`{{ ref.quote_sha256 }}`
{% endfor %}
{% endfor %}
{% endif %}
{% if gaps %}
## 缺口定位
{% for gap in gaps %}
- `{{ gap.code }}` · {{ gap.stage | evidence_text }}{% if gap.source_id %} · {{ gap.source_id | evidence_text }}{% endif %}{% if gap.relative_path %} · {{ gap.relative_path | evidence_text }}{% endif %}
{% endfor %}
{% endif %}
{% endif %}
