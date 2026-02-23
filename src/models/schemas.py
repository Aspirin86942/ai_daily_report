"""Pydantic 数据模型定义"""

from enum import Enum
from typing import Dict, List, Optional
from pydantic import BaseModel, Field


class ReportMode(str, Enum):
    """报告类型"""

    daily = "daily"
    weekly = "weekly"
    monthly = "monthly"


class DataSource(str, Enum):
    """数据来源策略"""

    db = "db"
    scan = "scan"


class WorkItem(BaseModel):
    """单项工作记录"""

    category: str = Field(description="工作类别 (如: 现场审计, 报告撰写, 数据分析)")
    content: str = Field(description="工作内容描述")
    status: str = Field(description="完成状态 (如: 已完成, 进行中)")
    quantitative: Optional[str] = Field(
        default=None, description="量化指标 (如: 审计3个项目, 发现5个问题)"
    )


class RiskItem(BaseModel):
    """风险问题记录"""

    severity: str = Field(description="严重程度: 高/中/低")
    description: str = Field(description="风险描述")


class DailyReportData(BaseModel):
    """日报结构化数据"""

    date: str = Field(description="日期 (YYYY-MM-DD)")
    summary: str = Field(description="今日工作概述 (1-2句话)")
    achievements: List[WorkItem] = Field(description="今日完成工作列表")
    risks: List[RiskItem] = Field(default_factory=list, description="风险与问题列表")
    plans: List[str] = Field(description="明日工作计划列表")
    yesterday_review: Optional[str] = Field(
        default=None, description="昨日计划完成情况评估"
    )


class CategorySummary(BaseModel):
    """分类汇总项 (周报/月报用)"""

    category: str = Field(description="工作类别")
    items: List[str] = Field(description="该类别下的工作项列表")
    total_count: int = Field(description="工作项总数")
    completion_rate: Optional[str] = Field(default=None, description="完成率")


class WeeklyReportData(BaseModel):
    """周报结构化数据"""

    week_label: str = Field(description="ISO 周标签 (如 2026-W05)")
    date_range: str = Field(description="日期范围 (如 2026-01-27 ~ 2026-02-02)")
    summary: str = Field(description="本周工作概述")
    category_summaries: List[CategorySummary] = Field(
        description="按类别汇总的工作列表"
    )
    risks: List[RiskItem] = Field(default_factory=list, description="风险与问题列表")
    key_achievements: List[str] = Field(description="本周重点成果")
    next_week_plans: List[str] = Field(description="下周工作计划")
    missing_days: List[str] = Field(
        default_factory=list, description="缺失日报的日期列表"
    )
    data_source: str = Field(description="数据来源 (db/scan)")


class MonthlyReportData(BaseModel):
    """月报结构化数据"""

    year_month: str = Field(description="年月 (YYYY-MM)")
    summary: str = Field(description="本月工作概述")
    category_summaries: List[CategorySummary] = Field(
        description="按类别汇总的工作列表"
    )
    risks: List[RiskItem] = Field(default_factory=list, description="风险与问题列表")
    statistics: Dict[str, str] = Field(
        default_factory=dict, description="统计指标 (灵活 KV)"
    )
    key_achievements: List[str] = Field(description="本月重点成果")
    next_month_plans: List[str] = Field(description="下月工作计划")
    missing_days: List[str] = Field(
        default_factory=list, description="缺失日报的日期列表"
    )
    data_source: str = Field(description="数据来源 (db/scan)")


class FileContext(BaseModel):
    """文件扫描结果"""

    file_path: str = Field(description="文件路径")
    file_type: str = Field(description="文件类型")
    content: str = Field(description="提取的文本内容")
    error: Optional[str] = Field(default=None, description="解析错误信息")


class ScanResult(BaseModel):
    """扫描汇总结果"""

    total_files: int = Field(description="扫描文件总数")
    success_count: int = Field(description="成功解析数量")
    error_count: int = Field(description="解析失败数量")
    contexts: List[FileContext] = Field(description="文件上下文列表")
