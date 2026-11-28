{% if section == "header" -%}
# {{ date }} 个人日报
{% elif section == "entry" -%}
[完整依据]({{ evidence_link }})
{% elif section == "body" -%}
{% if items -%}
{% for item in items -%}
- **{{ item.title | report_inline }}** · {% if item.kind == "next_step" %}下一步{% elif item.kind == "attention" %}关注{% else %}进展{% endif %}{% if item.statement_kind == "uncertain" %} · 待确认{% elif item.statement_kind == "suggestion" %} · 建议{% elif item.statement_kind == "user_statement" %} · 用户陈述{% endif %}

  {{ item.text | report_lines }} {% for id in item.source_ids %}[{{ labels[id] }}]({{ evidence_link }}#{{ labels[id] | lower }}){% if not loop.last %}、{% endif %}{% endfor %}
{% endfor -%}
{% elif no_changes -%}
本次扫描未选入目标日的有效文件，也没有用户补充。
{% else -%}
本次扫描存在覆盖缺口，详见完整依据。
{% endif -%}
{% elif section == "material" -%}
<a id="{{ anchor }}"></a>
{% if path %}### {{ label }} · {{ path | report_inline }}{% else %}### 用户补充原文 · {{ label }}{% endif %}

观察时间 {{ observed | shanghai_time }}{% if unverified %}。文字待核实{% endif %}。
{% for gap in omitted %}
- {{ gap | report_inline }}
{% endfor %}
{% for part in parts %}
{{ part.kind }}{% if part.range %} · {{ part.range | report_inline }}{% endif %}

{% if part.previous %}[查看相同原文](#{{ part.previous }})
{% else %}<a id="{{ part.anchor }}"></a>
<pre><code>{{ part.text | source_verbatim }}</code></pre>
{% endif %}{% endfor %}
{% elif section == "evidence" -%}
<a id="personal-evidence"></a>
# {{ date }} 个人日报 · 完整依据

## 材料范围

按目标日 00:00–24:00 的修改时间选取文件。原文来自扫描时的观察。当天中间版本、已删除文件及扫描前再次修改的文件可能缺失。

已观察 {{ selection.observed_files }} 个文件，检查 {{ selection.inspected_files }} 个，采纳 {{ selection.selected_files }} 个。{% if selection.incomplete_inventory %}扫描未覆盖全部文件。{% endif %}
{% for gap in gaps %}
- {% if gap.relative_path %}{{ gap.relative_path | report_inline }} · {% endif %}{{ gap.message | report_inline }}
{% endfor %}

## 采纳材料原文
{% endif %}
