{% if section == "material" -%}
<a id="{{ anchor }}"></a>
{% if path %}### {{ label }} · {{ path | report_inline }}{% else %}### 用户补充原文 · {{ label }}{% endif %}

观察时间 {{ observed | shanghai_time }}{% if unverified %}。文字待核实{% endif %}。
{% for part in parts %}
{{ part.kind }}{% if part.range %} · {{ part.range | report_inline }}{% endif %}

{% if part.previous %}[查看相同原文](#{{ part.previous }})
{% else %}<a id="{{ part.anchor }}"></a>
<pre><code>{{ part.text | source_literal }}</code></pre>
{% endif %}{% endfor %}
{% elif section == "heading" -%}
{% if required %}## 正文引用{% else %}## 补充材料{% endif %}
{% elif evidence_only -%}
<a id="personal-evidence"></a>
# {{ date }} 个人日报 · 完整依据

## 材料范围

已观察 {{ selection.observed_files }} 个文件，检查 {{ selection.inspected_files }} 个，选入 {{ selection.selected_files }} 个。{% if selection.incomplete_inventory %}扫描未覆盖全部文件。{% endif %}
{% for reason, count in selection.omissions | dictsort %}{% if count %}
- {% if reason == "duplicate" %}重复材料{% elif reason == "out_of_scope" %}范围外{% elif reason == "budget" %}预算未选{% elif reason == "read_failed" %}读取失败{% elif reason == "comparison_unavailable" %}无法比较{% elif reason == "no_usable_text" %}无可提取文字{% else %}{{ reason | report_inline }}{% endif %} {{ count }}。
{% endif %}{% endfor %}
{% if selection.omitted_paths %}记录的未选入路径：{% for path in selection.omitted_paths %}{{ path | report_inline }}{% if not loop.last %}、{% endif %}{% endfor %}。
{% endif %}{% for message, count in gaps | dictsort %}
- {{ message | report_inline }}{% if count > 1 %}（共 {{ count }} 条记录）{% endif %}
{% endfor %}
{% if items %}
## 事项与来源
{% for item in items %}
<a id="item-{{ loop.index }}"></a>
- 事项 {{ loop.index }} · {% for id in item.source_ids %}[{{ labels[id] }}](#{{ labels[id] | lower }}){% if not loop.last %}、{% endif %}{% endfor %}
{% endfor %}{% endif %}
{% else -%}
# {{ date }} 个人日报
{% if items %}
## 今日概览
{% for item in overview %}
- [{{ item.title | report_inline }}](#detail-{{ item.number }}){% if item.statement_kind == "uncertain" %} · 待确认{% elif item.statement_kind == "suggestion" %} · 建议{% endif %}
{%- endfor %}

## 事项详述
{% for item in items %}
<a id="detail-{{ loop.index }}"></a>
### {{ loop.index }}. {{ item.title | report_inline }}

**{% if item.kind == "next_step" %}下一步{% elif item.kind == "attention" %}关注事项{% else %}工作进展{% endif %} · {% if item.statement_kind == "suggestion" %}建议{% elif item.statement_kind == "uncertain" %}待确认{% elif item.statement_kind == "user_statement" %}用户陈述{% else %}材料记载{% endif %}**

{{ item.text | report_lines }}

依据编号 {% for id in item.source_ids %}{{ labels[id] }}{% if not loop.last %}、{% endif %}{% endfor %} · [查看依据]({{ evidence_link }}#item-{{ loop.index }})
{% endfor %}
{% elif quality == "no_changes" %}
本次扫描未选入目标日的有效文件，也没有用户补充。无法据此判断当天没有工作。
{% else %}
本次未能形成可用的事项总结。已选材料与处理缺口见完整依据。
{% endif %}
## 材料范围

按目标日 00:00–24:00 的修改时间选取文件，依据为次日扫描时的内容。文件中的安排和记录可能早于目标日。当天中间版本、已删除文件及扫描前再次修改的文件可能缺失。

[完整依据]({% if evidence_link %}{{ evidence_link }}{% else %}#personal-evidence{% endif %})
{% endif %}
