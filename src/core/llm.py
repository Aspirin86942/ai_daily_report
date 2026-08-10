"""LLM 交互模块。"""

import json
import os
import time
from datetime import date
from pathlib import Path
from typing import Optional

from pydantic import BaseModel, ValidationError

from ..core.config import config
from ..core.logger import setup_logger
from ..models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from ..utils.text_tools import format_period_report_context

logger = setup_logger()


class LLMClient:
    """LLM 客户端，支持 DeepSeek / OpenAI。"""

    def __init__(self):
        """初始化客户端并加载 prompt 模板。"""
        proxy_cfg = config.proxy_config
        if proxy_cfg.get("http"):
            os.environ["HTTP_PROXY"] = proxy_cfg["http"]
        if proxy_cfg.get("https"):
            os.environ["HTTPS_PROXY"] = proxy_cfg["https"]

        self.llm_cfg = config.llm_config
        self.provider = config.llm_provider

        # openai SDK 导入成本高（构建上千个 pydantic 模型），只在真正构造
        # 客户端时导入，避免拖慢任何仅使用 src.core 的命令启动。
        from openai import OpenAI

        base_url = str(self.llm_cfg.get("base_url") or "").strip()

        if self.provider == "deepseek":
            self.client = OpenAI(
                api_key=config.deepseek_api_key,
                base_url=base_url or "https://api.deepseek.com",
            )
        elif self.provider == "openai":
            kwargs: dict[str, str | None] = {"api_key": config.openai_api_key or None}
            if base_url:
                kwargs["base_url"] = base_url
            self.client = OpenAI(**kwargs)
        else:
            raise ValueError(
                f"Unsupported LLM provider: {self.provider}. Expected deepseek/openai."
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
        # 迁移/发布验收会处理合成文件上下文；硬开关必须在任何 SDK 调用前失败，
        # 防止测试命令意外把上下文发送到外部服务。
        if os.environ.get("AI_DAILY_TEST_FORBID_LLM") == "1":
            raise RuntimeError("LLM calls are prohibited in this process")
        for attempt in range(self.llm_cfg["max_retries"]):
            try:
                logger.info(
                    "调用 LLM (尝试 %s/%s)",
                    attempt + 1,
                    self.llm_cfg["max_retries"],
                )

                response = self.client.chat.completions.create(
                    model=self.llm_cfg["model_id"],
                    messages=[{"role": "user", "content": prompt}],
                    temperature=self.llm_cfg["temperature"],
                    max_tokens=self.llm_cfg["max_tokens"],
                    response_format={"type": "json_object"},
                )
                response_text = response.choices[0].message.content

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
