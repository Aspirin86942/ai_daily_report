"""历史记录管理服务"""
import json
from pathlib import Path
from typing import List, Optional
from datetime import date, datetime, timedelta

from ..models.schemas import DailyReportData, WeeklyReportData, MonthlyReportData
from ..core.config import config
from ..core.logger import setup_logger

logger = setup_logger()


class HistoryManager:
    """历史记录管理器"""

    def __init__(self):
        """初始化管理器"""
        self.db_dir = config.db_dir
        self.db_dir.mkdir(parents=True, exist_ok=True)

    def save_report(self, report: DailyReportData) -> Path:
        """保存日报到 JSON 文件

        Args:
            report: 日报数据

        Returns:
            保存的文件路径
        """
        file_path = self.db_dir / f"{report.date}.json"

        # 序列化为 JSON
        with open(file_path, 'w', encoding='utf-8') as f:
            json.dump(report.model_dump(), f, ensure_ascii=False, indent=2)

        logger.info(f"日报已保存: {file_path}")
        return file_path

    def get_yesterday_plan(self, target_date: Optional[datetime] = None) -> List[str]:
        """获取昨日计划

        Args:
            target_date: 目标日期 (默认为今天)

        Returns:
            昨日计划列表
        """
        if target_date is None:
            target_date = datetime.now()

        yesterday = target_date - timedelta(days=1)
        yesterday_str = yesterday.strftime('%Y-%m-%d')

        file_path = self.db_dir / f"{yesterday_str}.json"

        if not file_path.exists():
            logger.info(f"昨日日报不存在: {file_path}")
            return []

        try:
            with open(file_path, 'r', encoding='utf-8') as f:
                data = json.load(f)

            # 直接读取 plans 字段
            plans = data.get('plans', [])
            logger.info(f"读取昨日计划: {len(plans)} 项")
            return plans

        except Exception as e:
            logger.error(f"读取昨日计划失败: {e}")
            return []

    def get_month_reports(self, year_month: str) -> List[DailyReportData]:
        """获取指定月份的所有日报

        Args:
            year_month: 年月 (YYYY-MM)

        Returns:
            日报数据列表
        """
        reports = []

        # 遍历数据库目录
        for file_path in sorted(self.db_dir.glob(f"{year_month}-*.json")):
            try:
                with open(file_path, 'r', encoding='utf-8') as f:
                    data = json.load(f)

                report = DailyReportData(**data)
                reports.append(report)

            except Exception as e:
                logger.warning(f"读取日报失败 {file_path}: {e}")

        logger.info(f"读取 {year_month} 月日报: {len(reports)} 份")
        return reports

    def list_all_reports(self) -> List[str]:
        """列出所有日报日期

        Returns:
            日期列表 (YYYY-MM-DD)
        """
        dates = []
        for file_path in sorted(self.db_dir.glob("*.json")):
            date_str = file_path.stem
            dates.append(date_str)

        return dates

    def get_reports_in_range(
        self,
        start_date: date,
        end_date: date
    ) -> tuple[List[DailyReportData], List[str]]:
        """获取日期范围内的日报，并检测缺失工作日

        Args:
            start_date: 起始日期
            end_date: 结束日期

        Returns:
            (reports, missing_dates) — 日报列表和缺失日期列表
        """
        reports: list[DailyReportData] = []
        missing_dates: list[str] = []

        current = start_date
        while current <= end_date:
            # 仅检查工作日 (Mon-Fri)
            if current.weekday() < 5:
                date_str = current.isoformat()
                file_path = self.db_dir / f"{date_str}.json"

                if file_path.exists():
                    try:
                        with open(file_path, 'r', encoding='utf-8') as f:
                            data = json.load(f)
                        reports.append(DailyReportData(**data))
                    except Exception as e:
                        logger.warning(f"读取日报失败 {file_path}: {e}")
                        missing_dates.append(date_str)
                else:
                    missing_dates.append(date_str)

            current += timedelta(days=1)

        logger.info(f"范围 {start_date} ~ {end_date}: {len(reports)} 份日报, {len(missing_dates)} 天缺失")
        return reports, missing_dates

    def get_week_reports(
        self,
        year: int,
        week: int
    ) -> tuple[List[DailyReportData], List[str]]:
        """获取指定 ISO 周的日报

        Args:
            year: 年份
            week: ISO 周数

        Returns:
            (reports, missing_dates)
        """
        monday = date.fromisocalendar(year, week, 1)
        sunday = date.fromisocalendar(year, week, 7)
        return self.get_reports_in_range(monday, sunday)

    def save_weekly_report(self, report: WeeklyReportData) -> Path:
        """保存周报到 JSON 文件

        Args:
            report: 周报数据

        Returns:
            保存的文件路径
        """
        weekly_dir = self.db_dir / "weekly"
        weekly_dir.mkdir(parents=True, exist_ok=True)

        file_path = weekly_dir / f"{report.week_label}.json"
        with open(file_path, 'w', encoding='utf-8') as f:
            json.dump(report.model_dump(), f, ensure_ascii=False, indent=2)

        logger.info(f"周报已保存: {file_path}")
        return file_path

    def save_monthly_report(self, report: MonthlyReportData) -> Path:
        """保存月报到 JSON 文件

        Args:
            report: 月报数据

        Returns:
            保存的文件路径
        """
        monthly_dir = self.db_dir / "monthly"
        monthly_dir.mkdir(parents=True, exist_ok=True)

        file_path = monthly_dir / f"{report.year_month}.json"
        with open(file_path, 'w', encoding='utf-8') as f:
            json.dump(report.model_dump(), f, ensure_ascii=False, indent=2)

        logger.info(f"月报已保存: {file_path}")
        return file_path
