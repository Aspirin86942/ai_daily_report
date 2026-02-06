# {{ report.year_month }} 审计月报

## 本月工作概述
{{ report.summary }}

## 工作分类汇总
{% for cat in report.category_summaries %}
### {{ loop.index }}. {{ cat.category }}
{% for item in cat.items %}
- {{ item }}
{% endfor %}
- **小计**: {{ cat.total_count }} 项{% if cat.completion_rate %} | 完成率: {{ cat.completion_rate }}{% endif %}

{% endfor %}

{% if report.statistics %}
## 统计指标
| 指标 | 数值 |
|------|------|
{% for key, value in report.statistics.items() %}
| {{ key }} | {{ value }} |
{% endfor %}
{% endif %}

## 重点成果
{% for achievement in report.key_achievements %}
{{ loop.index }}. {{ achievement }}
{% endfor %}

{% if report.risks %}
## 风险与问题
{% for risk in report.risks %}
### {{ loop.index }}. [{{ risk.severity }}] {{ risk.description }}

{% endfor %}
{% endif %}

## 下月工作计划
{% for plan in report.next_month_plans %}
{{ loop.index }}. {{ plan }}
{% endfor %}

{% if report.missing_days %}
## 缺失日报
{% for day in report.missing_days %}
- {{ day }}
{% endfor %}
{% endif %}

---
*数据来源: {{ report.data_source }} | 报告生成时间: {{ now }}*
