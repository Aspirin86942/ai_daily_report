"""LLM 交互模块。"""

import json
import os
import time
from datetime import date
from pathlib import Path
from typing import Optional

from google import genai
from google.genai import types
from openai import OpenAI
from pydantic import BaseModel, ValidationError

from ..core.config import config
from ..core.logger import setup_logger
from ..models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from ..utils.text_tools import format_period_report_context

logger = setup_logger()


class LLMClient:
    """LLM 客户端，支持 Gemini 和 OpenAI。"""

    def __init__(self):
        """初始化客户端并加载 prompt 模板。"""
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

        template_dir = Path(__file__).parent.parent.parent / "templates"
        self.prompt_templates: dict[str, str] = {}
        for name in ("system_prompt", "weekly_prompt", "monthly_prompt"):
            path = template_dir / f"{name}.md"
            with open(path, "r", encoding="utf-8") as file:
                self.prompt_templates[name] = file.read()

    def _call_llm_with_json(
        self, prompt: str, response_model: type[BaseModel]
    ) -> BaseModel:
        """调用 LLM 并将响应校验为指定 Pydantic 模型。"""
        for attempt in range(self.llm_cfg["max_retries"]):
            try:
                logger.info(
                    "调用 LLM (尝试 %s/%s)",
                    attempt + 1,
                    self.llm_cfg["max_retries"],
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

                logger.info("LLM 返回长度: %s 字符", len(response_text))
                result = response_model.model_validate_json(response_text)
                logger.info("%s 校验通过", response_model.__name__)
                return result
            except ValidationError as exc:
                logger.error("JSON 校验失败: %s", exc)
                if attempt == self.llm_cfg["max_retries"] - 1:
                    raise Exception(f"JSON 校验失败: {exc}") from exc
            except Exception as exc:
                logger.warning("LLM 调用失败: %s", exc)
                if attempt == self.llm_cfg["max_retries"] - 1:
                    raise Exception(
                        f"LLM 调用失败 (已重试 {self.llm_cfg['max_retries']} 次): {exc}"
                    ) from exc

                wait_time = 2**attempt
                logger.info("等待 %s 秒后重试", wait_time)
                time.sleep(wait_time)

        raise RuntimeError("LLM 调用重试流程异常结束")

    def generate_report(
        self,
        user_input: str,
        file_context: str,
        yesterday_plan: Optional[str] = None,
    ) -> DailyReportData:
        """生成日报。"""
        schema_json = DailyReportData.model_json_schema()
        yesterday_text = yesterday_plan or "无"

        prompt = self.prompt_templates["system_prompt"].format(
            schema=json.dumps(schema_json, ensure_ascii=False, indent=2),
            user_input=user_input,
            yesterday_plan=yesterday_text,
            file_context=file_context,
        )

        report_data: DailyReportData = self._call_llm_with_json(prompt, DailyReportData)
        report_data.date = date.today().isoformat()
        return report_data

    def generate_weekly_report(
        self,
        reports: list[DailyReportData],
        file_context: str,
        year: int,
        week: int,
        missing_days: list[str],
        data_source: str,
    ) -> WeeklyReportData:
        """生成周报。"""
        schema_json = WeeklyReportData.model_json_schema()
        week_label = f"{year}-W{week:02d}"
        reports_summary = format_period_report_context(reports)
        missing_text = "无" if not missing_days else "、".join(missing_days)

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
        reports: list[DailyReportData],
        file_context: str,
        year_month: str,
        missing_days: list[str],
        data_source: str,
    ) -> MonthlyReportData:
        """生成月报。"""
        schema_json = MonthlyReportData.model_json_schema()
        reports_summary = format_period_report_context(reports)
        missing_text = "无" if not missing_days else "、".join(missing_days)

        prompt = self.prompt_templates["monthly_prompt"].format(
            schema=json.dumps(schema_json, ensure_ascii=False, indent=2),
            year_month=year_month,
            reports_summary=reports_summary,
            file_context=file_context,
            missing_days=missing_text,
            data_source=data_source,
        )

        return self._call_llm_with_json(prompt, MonthlyReportData)
