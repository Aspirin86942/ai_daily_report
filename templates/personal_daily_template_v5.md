# {{ date }} 个人日报

{% if quality == "no_changes" %}本次检查未发现目标日的有效变化，也没有用户补充。
{% elif items | length == 0 %}本次未能形成可用的事项总结。材料与处理缺口见下方说明。
{% endif %}
{% for item in items %}
## {{ item.title | evidence_text }}

{% if item.statement_kind == "suggestion" %}**建议：** {% elif item.statement_kind == "uncertain" %}**待确认：** {% elif item.statement_kind == "user_statement" %}**用户补充：** {% endif %}{{ item.text | evidence_text }}

来源：{% for id in item.source_ids %}{% for source in sources_view %}{% if source.id == id %}[{{ source.label }}](#{{ source.label | lower }}) {% endif %}{% endfor %}{% endfor %}
{% endfor %}
{% if user_notes %}
## 用户补充原文

{{ user_notes | evidence_text }}
{% endif %}
## 材料范围

已观察 {{ selection.observed_files }} 个文件，检查 {{ selection.inspected_files }} 个，选入 {{ selection.selected_files }} 个。{% if selection.incomplete_inventory %}观察尚不完整，上述数量为已观察范围。{% endif %}
{% for reason, count in selection.omissions | dictsort %}
{% if count %}{% if reason == "duplicate" %}重复材料{% elif reason == "out_of_scope" %}范围外{% elif reason == "budget" %}预算未选{% elif reason == "read_failed" %}读取失败{% elif reason == "comparison_unavailable" %}无法比较{% else %}未展开{% endif %}：{{ count }}。
{% endif %}{% endfor %}
{% for gap in gaps %}
- {{ gap.message | evidence_text }}
{% endfor %}
{% if sources_view %}
## 关键依据
{% for source in sources_view %}
<a id="{{ source.label | lower }}"></a>
### {{ source.label }} · {{ source.path | evidence_text }}

观察时间：{{ source.observed | shanghai_time }}
{% for ref in source.refs or [] %}
{% if ref.side == "before" %}变更前{% elif ref.side == "after" %}变更后{% else %}本次读取{% endif %} · {{ ref.unit_id | evidence_text }}

{{ ref.quote | evidence_text }}

内容指纹：`{{ ref.quote_sha256 }}`
{% endfor %}
{% endfor %}
{% endif %}
