from enum import Enum

from pydantic import BaseModel, ConfigDict, Field


class ReportMode(str, Enum):
    daily = "daily"
    weekly = "weekly"
    monthly = "monthly"


class DataSource(str, Enum):
    db = "db"
    scan = "scan"


class DailyReportData(BaseModel):
    model_config = ConfigDict(extra="forbid")
    date: str = Field(description="日期 (YYYY-MM-DD)")
    completed_work: str = Field(description="今日工作完成内容，使用自然段，不分条")
    work_summary: str = Field(description="今日工作小结，使用自然段")
    next_plan: str = Field(description="明日工作计划，使用自然段，不分条")


class WeeklyReportData(BaseModel):
    model_config = ConfigDict(extra="forbid")
    week_label: str = Field(description="ISO 周标签，例如 2026-W14")
    date_range: str = Field(description="日期范围，例如 2026-03-30 ~ 2026-04-05")
    overview: str = Field(description="本周总览，使用自然段")
    completed_work: str = Field(description="本周完成内容，使用自然段")
    work_summary: str = Field(description="本周工作小结，使用自然段")
    next_plan: str = Field(description="下周工作计划，使用自然段")


class MonthlyReportData(BaseModel):
    model_config = ConfigDict(extra="forbid")
    year_month: str = Field(description="年月 (YYYY-MM)")
    overview: str = Field(description="本月总览，使用自然段")
    completed_work: str = Field(description="本月完成内容，使用自然段")
    work_summary: str = Field(description="本月工作小结，使用自然段")
    next_plan: str = Field(description="下月工作计划，使用自然段")


class FileContext(BaseModel):
    file_path: str = Field(description="文件路径")
    file_type: str = Field(description="文件类型")
    content: str = Field(description="抽取文本")
    error: str = Field(description="发现的问题摘要")


class ScanResult(BaseModel):
    total_files: int = Field(description="扫描文件总数")
    success_count: int = Field(description="成功解析数")
    error_count: int = Field(description="失败解析数")
    contexts: list[FileContext] = Field(description="文件级上下文")
