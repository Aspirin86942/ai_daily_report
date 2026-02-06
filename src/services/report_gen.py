"""报告生成服务"""
from pathlib import Path
from datetime import datetime
from jinja2 import Environment, FileSystemLoader

from ..models.schemas import DailyReportData, WeeklyReportData, MonthlyReportData
from ..core.config import config
from ..core.logger import setup_logger

logger = setup_logger()


class ReportGenerator:
    """报告生成器"""

    def __init__(self):
        """初始化生成器"""
        self.reports_dir = config.reports_dir
        self.reports_dir.mkdir(parents=True, exist_ok=True)

        # 初始化 Jinja2 环境
        template_dir = Path(__file__).parent.parent.parent / "templates"
        self.env = Environment(loader=FileSystemLoader(str(template_dir)))

    def render_markdown(self, report: DailyReportData) -> str:
        """渲染日报 Markdown

        Args:
            report: 日报数据

        Returns:
            Markdown 文本
        """
        template = self.env.get_template('report_template.md')
        return template.render(report=report)

    def save_markdown(
        self,
        content: str,
        report_date: str,
        output_dir: Path = None
    ) -> Path:
        """保存日报 Markdown 文件

        Args:
            content: Markdown 内容
            report_date: 报告日期 (YYYY-MM-DD)
            output_dir: 输出目录 (默认使用配置的 reports_dir)

        Returns:
            保存的文件路径
        """
        if output_dir is None:
            output_dir = self.reports_dir

        # 创建月份子目录
        date_obj = datetime.strptime(report_date, '%Y-%m-%d')
        month_dir = output_dir / date_obj.strftime('%Y-%m')
        month_dir.mkdir(parents=True, exist_ok=True)

        # 保存文件
        file_path = month_dir / f"{report_date}.md"
        with open(file_path, 'w', encoding='utf-8') as f:
            f.write(content)

        logger.info(f"Markdown 已保存: {file_path}")
        return file_path

    def render_weekly_markdown(self, report: WeeklyReportData) -> str:
        """渲染周报 Markdown

        Args:
            report: 周报数据

        Returns:
            Markdown 文本
        """
        template = self.env.get_template('weekly_template.md')
        return template.render(report=report, now=datetime.now().strftime('%Y-%m-%d %H:%M'))

    def render_monthly_markdown(self, report: MonthlyReportData) -> str:
        """渲染月报 Markdown

        Args:
            report: 月报数据

        Returns:
            Markdown 文本
        """
        template = self.env.get_template('monthly_template.md')
        return template.render(report=report, now=datetime.now().strftime('%Y-%m-%d %H:%M'))

    def save_weekly_markdown(self, content: str, year: int, week: int) -> Path:
        """保存周报 Markdown 文件

        Args:
            content: Markdown 内容
            year: 年份
            week: ISO 周数

        Returns:
            保存的文件路径
        """
        weekly_dir = self.reports_dir / "weekly"
        weekly_dir.mkdir(parents=True, exist_ok=True)

        week_label = f"{year}-W{week:02d}"
        file_path = weekly_dir / f"{week_label}.md"
        with open(file_path, 'w', encoding='utf-8') as f:
            f.write(content)

        logger.info(f"周报 Markdown 已保存: {file_path}")
        return file_path

    def save_monthly_markdown(self, content: str, year_month: str) -> Path:
        """保存月报 Markdown 文件

        Args:
            content: Markdown 内容
            year_month: 年月 (YYYY-MM)

        Returns:
            保存的文件路径
        """
        monthly_dir = self.reports_dir / "monthly"
        monthly_dir.mkdir(parents=True, exist_ok=True)

        file_path = monthly_dir / f"{year_month}.md"
        with open(file_path, 'w', encoding='utf-8') as f:
            f.write(content)

        logger.info(f"月报 Markdown 已保存: {file_path}")
        return file_path
