# {{ report.date }} 工作日报

{% if report.historical_reconstruction %}> 本报告为历史补录，根据冻结时可取得的材料整理；各事项的实际状态以材料中的日期和版本为准。

{% endif %}{% if report.quality == 'partial' %}> 本日报只覆盖已取得并成功处理的材料，相关缺口见来源附录。

{% endif %}{% if report.missed_dates %}> 待补日报日期：{{ report.missed_dates | join('、') }}。

{% endif %}{% if report.quality != 'no_changes' and (report.speed.sources_total > 0 or report.speed.facts_total > 0) %}> 本报告基于 {{ report.speed.sources_total }} 份材料（读取成功 {{ report.speed.sources_usable }}、失败 {{ report.speed.sources_failed }}），提取事实 {{ report.speed.facts_total }} 条（确定 {{ report.speed.facts_confirmed }}、待确认 {{ report.speed.facts_pending }}）。{% if report.speed.gaps_total > 0 %}覆盖缺口 {{ report.speed.gaps_codes }} 类共 {{ report.speed.gaps_total }} 项。{% endif %}

{% endif %}## 内容概览

{% if report.quality == 'no_changes' %}在目标日期内未发现修改的文件，也未收到工作补充。该结果仅反映材料情况。
{% elif not report.content %}已读取的材料未形成可确认的具体事项，暂不扩写工作进展。
{% else %}{% for item in report.overview_view %}- {{ item.paragraphs[0] | evidence_text }} {{ item.refs }}
{% endfor %}{% if not report.content.overview %}当前材料中的内容仍待确认，详见下方待确认事项。
{% endif %}
{% if report.toc %}## 事项目录

{% for item in report.toc %}- [{{ item.title | evidence_text }}](#{{ item.anchor }})
{% endfor %}
{% endif %}## 事项详情

{% for matter in report.matters_view %}<a id="{{ matter.anchor }}"></a>
### {{ matter.title | evidence_text }}

{% if matter.background %}{% for paragraph in matter.background.paragraphs %}{{ paragraph | evidence_text }}{% if loop.last %} {{ matter.background.refs }}{% endif %}

{% endfor %}{% endif %}{% for detail in matter.details %}{% for paragraph in detail.paragraphs %}{{ paragraph | evidence_text }}{% if loop.last %} {{ detail.refs }}{% endif %}

{% endfor %}{% endfor %}{% if matter.status %}**当前状态：** {{ matter.status.paragraphs[0] | evidence_text }} {{ matter.status.refs }}

{% endif %}{% endfor %}## 待确认

{% for item in report.pending_view %}- {{ item.paragraphs[0] | evidence_text }} {{ item.refs }}
{% else %}本次材料未列出其他待确认事项。
{% endfor %}{% endif %}
{% if report.user_notes %}## 补充说明

{{ report.user_notes | evidence_text }}

{% endif %}## 来源与覆盖附录

材料范围：{{ report.root_alias | evidence_text }}；时间窗口为 {{ report.window_start_utc | shanghai_time }} 至 {{ report.window_end_utc | shanghai_time }}，不含右端时间。

{% for source in report.sources %}<a id="{{ report.source_labels[source.source_id] | lower }}"></a>
### {{ report.source_labels[source.source_id] }} · {% if source.relative_path %}{{ source.relative_path | evidence_text }}{% else %}用户补充{% endif %}

- 读取状态：{% if source.parse_status == 'success' %}已读取{% elif source.parse_status == 'timeout' %}读取超时{% elif source.parse_status == 'not_parsed' %}未读取{% else %}读取失败{% endif %}；摘录 {{ source.excerpt_chars }} 字，成功用于事实提取 {{ source.model_chars }} 字。
{% if source.parse_truncated or source.excerpt_truncated or source.model_truncated %}- 覆盖范围有限，未读取或未发送的内容不属于本次结论依据。
{% endif %}{% if source.content_sha256 %}- 原文件 SHA-256：`{{ source.content_sha256 }}`。
{% endif %}{% if source.parser_backend %}- 解析器：`{{ source.parser_backend }}`。
{% endif %}
{% else %}本次没有文件来源。
{% endfor %}{% if report.gaps %}### 覆盖缺口

{% for gap in report.gaps_grouped %}- {% if gap.label_summary %}{{ gap.label_summary }}：{% endif %}{{ gap.message | evidence_text }}（`{{ gap.code }}`）
{% endfor %}{% endif %}
---
生成时间：{{ report.generated_at | shanghai_time }}。
