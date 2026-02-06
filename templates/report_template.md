# {{ report.date }} 审计日报

## 今日工作概述
{{ report.summary }}

{% if report.yesterday_review %}
## 昨日计划完成情况
{{ report.yesterday_review }}

{% endif %}
## 今日完成工作
{% for item in report.achievements %}
### {{ loop.index }}. {{ item.category }}
- **内容**: {{ item.content }}
- **状态**: {{ item.status }}
{% if item.quantitative %}
- **量化指标**: {{ item.quantitative }}
{% endif %}

{% endfor %}

{% if report.risks %}
## 风险与问题
{% for risk in report.risks %}
### {{ loop.index }}. [{{ risk.severity }}] {{ risk.description }}

{% endfor %}
{% endif %}

## 明日工作计划
{% for plan in report.plans %}
{{ loop.index }}. {{ plan }}
{% endfor %}

---
*报告生成时间: {{ report.date }}*
