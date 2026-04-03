"""LLM 交互模块"""

import os
import json
import time
from typing import List, Optional
from pathlib import Path
from datetime import date
from google import genai
from google.genai import types
from openai import OpenAI
from pydantic import BaseModel, ValidationError

from ..models.schemas import DailyReportData, WeeklyReportData, MonthlyReportData
from ..core.config import config
from ..core.logger import setup_logger

logger = setup_logger()


class LLMClient:
    """LLM 客户端 (支持 Gemini/OpenAI)"""

    def __init__(self):
        """初始化 LLM 客户端"""
        # 设置代理
        proxy_cfg = config.proxy_config
        if proxy_cfg.get("http"):
            os.environ["HTTP_PROXY"] = proxy_cfg["http"]
        if proxy_cfg.get("https"):
            os.environ["HTTPS_PROXY"] = proxy_cfg["https"]

        self.llm_cfg = config.llm_config
        self.provider = config.llm_provider

        if self.provider == "gemini":
            self.client = genai.Client(api_key=config.google_api_key)
        elif self.provider == "openai":
            self.client = OpenAI(api_key=config.openai_api_key or None)
        else:
            raise ValueError(
                f"Unsupported LLM provider: {self.provider}. Expected gemini/openai."
            )

        # 加载所有 prompt 模板
        template_dir = Path(__file__).parent.parent.parent / "templates"
        self.prompt_templates: dict[str, str] = {}
        for name in ("system_prompt", "weekly_prompt", "monthly_prompt"):
            path = template_dir / f"{name}.md"
            with open(path, "r", encoding="utf-8") as f:
                self.prompt_templates[name] = f.read()

    def _call_llm_with_json(
        self, prompt: str, response_model: type[BaseModel]
    ) -> BaseModel:
        """调用 LLM 并校验 JSON 响应 (含重试逻辑)

        Args:
            prompt: 完整的 prompt 文本
            response_model: Pydantic 模型类

        Returns:
            校验通过的 Pydantic 模型实例

        Raises:
            Exception: 重试耗尽后仍失败
        """
        for attempt in range(self.llm_cfg["max_retries"]):
            try:
                logger.info(
                    f"调用 LLM (尝试 {attempt + 1}/{self.llm_cfg['max_retries']})"
                )

                if self.provider == "gemini":
                    response = self.client.models.generate_content(
                        model=self.llm_cfg["model_id"],
                        contents=prompt,
                        config=types.GenerateContentConfig(
                            temperature=self.llm_cfg["temperature"],
                            max_output_tokens=self.llm_cfg["max_tokens"],
                            response_mime_type="application/json",
                        ),
                    )

                    response_text = response.text

                elif self.provider == "openai":
                    schema_json = response_model.model_json_schema()
                    response = self.client.responses.create(
                        model=self.llm_cfg["model_id"],
                        input=prompt,
                        temperature=self.llm_cfg["temperature"],
                        max_output_tokens=self.llm_cfg["max_tokens"],
                        text={
                            "format": {
                                "type": "json_schema",
                                "name": response_model.__name__,
                                "schema": schema_json,
                                "strict": False,
                            }
                        },
                    )
                    response_text = response.output_text

                else:
                    raise ValueError(
                        f"Unsupported LLM provider: {self.provider}. Expected gemini/openai."
                    )
                logger.info(f"LLM 返回: {len(response_text)} 字符")

                # Pydantic 校验
                result = response_model.model_validate_json(response_text)
                logger.info(f"{response_model.__name__} 校验通过")
                return result

            except ValidationError as e:
                logger.error(f"JSON 校验失败: {e}")
                if attempt == self.llm_cfg["max_retries"] - 1:
                    raise Exception(f"JSON 校验失败: {e}")

            except Exception as e:
                logger.warning(f"LLM 调用失败: {e}")
                if attempt == self.llm_cfg["max_retries"] - 1:
                    raise Exception(
                        f"LLM 调用失败 (已重试 {self.llm_cfg['max_retries']} 次): {e}"
                    )

                # 指数退避
                wait_time = 2**attempt
                logger.info(f"等待 {wait_time} 秒后重试")
                time.sleep(wait_time)

    def generate_report(
        self,
        user_input: str,
        file_context: str,
        yesterday_plan: Optional[str] = None,
    ) -> DailyReportData:
        """生成日报

        Args:
            user_input: 用户口述内容
            file_context: 文件扫描结果
            yesterday_plan: 昨日计划参考文本

        Returns:
            结构化日报数据
        """
        schema_json = DailyReportData.model_json_schema()
        yesterday_text = yesterday_plan or "无"

        prompt = self.prompt_templates["system_prompt"].format(
            schema=json.dumps(schema_json, ensure_ascii=False, indent=2),
            user_input=user_input,
            yesterday_plan=yesterday_text,
            file_context=file_context,
        )

        report_data: DailyReportData = self._call_llm_with_json(prompt, DailyReportData)

        # 强制使用当前日期
        report_data.date = date.today().isoformat()

        return report_data

    def generate_weekly_report(
        self,
        reports: List[DailyReportData],
        file_context: str,
        year: int,
        week: int,
        missing_days: List[str],
        data_source: str,
    ) -> WeeklyReportData:
        """生成周报

        Args:
            reports: 本周日报列表
            file_context: 文件扫描结果
            year: 年份
            week: ISO 周数
            missing_days: 缺失日报的日期列表
            data_source: 数据来源 (db/scan)

        Returns:
            结构化周报数据
        """
        schema_json = WeeklyReportData.model_json_schema()
        week_label = f"{year}-W{week:02d}"
        reports_summary = self._summarize_reports(reports) if reports else "无日报数据"
        missing_text = ", ".join(missing_days) if missing_days else "无"

        prompt = self.prompt_templates["weekly_prompt"].format(
            schema=json.dumps(schema_json, ensure_ascii=False, indent=2),
            week_label=week_label,
            reports_summary=reports_summary,
            file_context=file_context,
            missing_days=missing_text,
            data_source=data_source,
        )

        return self._call_llm_with_json(prompt, WeeklyReportData)

    def generate_monthly_report(
        self,
        reports: List[DailyReportData],
        file_context: str,
        year_month: str,
        missing_days: List[str],
        data_source: str,
    ) -> MonthlyReportData:
        """生成月报

        Args:
            reports: 本月日报列表
            file_context: 文件扫描结果
            year_month: 年月 (YYYY-MM)
            missing_days: 缺失日报的日期列表
            data_source: 数据来源 (db/scan)

        Returns:
            结构化月报数据
        """
        schema_json = MonthlyReportData.model_json_schema()
        reports_summary = self._summarize_reports(reports) if reports else "无日报数据"
        missing_text = ", ".join(missing_days) if missing_days else "无"

        prompt = self.prompt_templates["monthly_prompt"].format(
            schema=json.dumps(schema_json, ensure_ascii=False, indent=2),
            year_month=year_month,
            reports_summary=reports_summary,
            file_context=file_context,
            missing_days=missing_text,
            data_source=data_source,
        )

        return self._call_llm_with_json(prompt, MonthlyReportData)

    def _summarize_reports(self, reports: List[DailyReportData]) -> str:
        """将日报列表压缩为摘要文本

        Args:
            reports: 日报列表

        Returns:
            摘要文本 (每份约 200 字)
        """
        parts: list[str] = []
        for r in reports:
            # 工作项摘要
            work_items = "; ".join(
                f"[{a.category}] {a.content} ({a.status})" for a in r.achievements
            )
            # 风险摘要
            risk_items = (
                "; ".join(f"[{ri.severity}] {ri.description}" for ri in r.risks)
                if r.risks
                else "无"
            )

            part = (
                f"## {r.date}\n"
                f"概述: {r.summary}\n"
                f"工作: {work_items}\n"
                f"风险: {risk_items}\n"
                f"明日计划: {', '.join(r.plans)}"
            )
            parts.append(part)

        return "\n\n".join(parts)
