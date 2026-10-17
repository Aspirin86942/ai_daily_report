use crate::{
    config::AppConfig,
    types::{self, Period, ReportMode, ReportRequest, ReportSource, RunContext},
    ReportError,
};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(version, about = "AI Daily Report — 文件证据日报、周报与月报")]
pub struct Cli {
    #[arg(long, global = true, help = "配置目录；默认使用程序旁的 config")]
    pub config_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 保存观察到的文件版本并处理归档解析队列
    Collect(crate::archive::cli::CollectArgs),
    /// 查询与维护长期档案
    Archive {
        #[arg(long, value_enum, default_value = "json", global = true)]
        format: crate::archive::cli::Format,
        #[command(subcommand)]
        command: crate::archive::cli::ArchiveCommand,
    },
    /// 只读汇总采集与模型调用指标
    Stats(crate::archive::cli::StatsArgs),
    /// 生成已结束自然日的文件证据日报
    Daily(DailyArgs),
    /// 汇总一周报告，须明确选择 db 或 scan 来源
    Weekly(PeriodArgs),
    /// 汇总一个月报告，须明确选择 db 或 scan 来源
    Monthly(PeriodArgs),
    /// 查看历史报告
    List,
    /// 查看报告运行及调用状态
    Runs {
        #[arg(long, value_parser = types::parse_date)]
        target_date: Option<NaiveDate>,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },
    /// 从数据库恢复原正文，不调用模型
    RepairOutput {
        #[arg(long, value_parser = types::parse_date)]
        target_date: NaiveDate,
    },
    /// 检查配置、模板、数据库及解析进程
    #[command(alias = "check-config")]
    Doctor {
        #[arg(long)]
        strict: bool,
        #[arg(long, help = "发送计入审计与预算的合成模型请求")]
        check_model: bool,
    },
    /// 批量补跑或仅执行输入预检
    Backfill(BackfillArgs),
    /// 固定输入、生成候选、发布或恢复覆盖修复作业
    Coverage {
        #[command(subcommand)]
        command: CoverageCommand,
    },
    /// 校验显式提供的 OCR 文字与原文件身份
    Ocr {
        #[command(subcommand)]
        command: OcrCommand,
    },
    /// 生成 Windows 计划任务定义
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// 核对到期材料；明确 --execute 才执行清理
    Cleanup {
        #[arg(long)]
        execute: bool,
    },
    /// 创建 SQLite 一致备份，目标文件必须尚不存在
    Backup {
        #[arg(long)]
        output: PathBuf,
    },
    /// 对指定扫描库创建一致归档，不修改原数据库
    ArchiveScanner {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        archive_dir: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct DailyArgs {
    #[arg(long, value_enum)]
    pub source: Option<ReportSource>,
    #[arg(long)]
    pub ocr_manifest: Option<PathBuf>,
    #[arg(short = 'i', long)]
    pub input: Option<String>,
    #[arg(long, conflicts_with_all = ["dry_run", "scheduled"])]
    pub no_save: bool,
    #[arg(long, value_parser = types::parse_date, conflicts_with_all = ["date", "scheduled"])]
    pub target_date: Option<NaiveDate>,
    #[arg(long, value_parser = types::parse_date, conflicts_with_all = ["target_date", "scheduled"], help = "--target-date 的弃用别名")]
    pub date: Option<NaiveDate>,
    #[arg(long, conflicts_with_all = ["target_date", "date", "no_save", "dry_run", "regenerate"])]
    pub scheduled: bool,
    #[arg(long, value_parser = ["Asia/Shanghai"])]
    pub timezone: Option<String>,
    #[arg(long, conflicts_with_all = ["no_save", "scheduled", "regenerate"])]
    pub dry_run: bool,
    #[arg(long, requires = "reason", conflicts_with_all = ["scheduled", "dry_run"])]
    pub regenerate: bool,
    #[arg(long, requires = "regenerate")]
    pub reason: Option<String>,
}

#[derive(Debug, Args)]
pub struct PeriodArgs {
    pub period: Option<String>,
    #[arg(long, value_enum)]
    pub source: ReportSource,
    #[arg(short = 'i', long)]
    pub input: Option<String>,
    #[arg(long)]
    pub no_save: bool,
}

#[derive(Debug, Args)]
pub struct BackfillArgs {
    #[arg(long, value_parser = types::parse_date)]
    pub start_date: Option<NaiveDate>,
    #[arg(long, value_parser = types::parse_date)]
    pub end_date: Option<NaiveDate>,
    #[arg(long)]
    pub preflight: Option<PathBuf>,
    #[arg(long)]
    pub call_budget: Option<u64>,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=4))]
    pub workers: u8,
    #[arg(long)]
    pub ocr_manifest: Option<PathBuf>,
    #[arg(long)]
    pub only_no_model: bool,
    #[arg(long)]
    pub only_failed: bool,
    #[arg(long)]
    pub output_tokens: Option<u64>,
    #[arg(long)]
    pub repair_coverage: bool,
    #[arg(long, value_delimiter = ',', value_parser = types::parse_date)]
    pub dates: Vec<NaiveDate>,
    #[arg(long, value_delimiter = ',')]
    pub gap_codes: Vec<String>,
    #[arg(long)]
    pub scope_root: Option<PathBuf>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub refresh_preflight: bool,
    #[arg(long)]
    pub plan: Option<PathBuf>,
    #[arg(long)]
    pub generate_candidates: bool,
    #[arg(long)]
    pub publish_candidates: Option<PathBuf>,
    #[arg(long)]
    pub token_budget: Option<u64>,
    #[arg(long)]
    pub cost_budget_microunits: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub enum CoverageCommand {
    Plan {
        #[arg(long, required = true, value_delimiter = ',', value_parser = types::parse_date)]
        dates: Vec<NaiveDate>,
        #[arg(long, required = true, value_delimiter = ',')]
        gap_codes: Vec<String>,
        #[arg(long)]
        scope_root: PathBuf,
        #[arg(long)]
        call_budget: u64,
        #[arg(long)]
        token_budget: u64,
        #[arg(long)]
        cost_budget_microunits: Option<u64>,
        #[arg(long)]
        refresh_preflight: bool,
    },
    Generate {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=4))]
        workers: u8,
    },
    Publish {
        #[arg(long)]
        candidates: PathBuf,
    },
    Recover {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        authorization_note: String,
        #[arg(long)]
        calls_limit: Option<u64>,
        #[arg(long)]
        revise_runtime: bool,
    },
    Amend {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum OcrCommand {
    Validate {
        #[arg(long)]
        manifest: PathBuf,
    },
    /// 显式运行已批准的本地 OCR 引擎，输出待核实清单
    Run {
        #[arg(long)]
        engine: PathBuf,
        #[arg(long, allow_hyphen_values = true)]
        engine_arg: Vec<String>,
        #[arg(long)]
        file: PathBuf,
        #[arg(long, required = true, value_delimiter = ',')]
        pages: Vec<u64>,
        #[arg(long)]
        output_dir: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    Plan {
        #[arg(long,default_value="daily",value_parser=["daily","collect"])]
        kind: String,
        #[arg(long)]
        sid: String,
    },
}

impl Command {
    pub fn report_request(
        &self,
        config: &AppConfig,
        now: DateTime<Utc>,
        run_id: String,
        fingerprint: String,
    ) -> Result<ReportRequest, ReportError> {
        let today = now.with_timezone(&types::shanghai()).date_naive();
        let (period, input, save, dry_run, scheduled, regeneration_reason) = match self {
            Self::Daily(args) => {
                let target = types::target_date(
                    now,
                    args.target_date.or(args.date),
                    args.scheduled,
                    config.schedule.due_time()?,
                )?;
                let reason = args.reason.as_deref().map(str::trim).map(str::to_owned);
                if args.regenerate && reason.as_deref().is_none_or(str::is_empty) {
                    return Err(ReportError::new(
                        "REGENERATION_REASON_REQUIRED",
                        "重新生成须提供非空原因",
                    ));
                }
                (
                    Period {
                        mode: ReportMode::Daily,
                        source: args.source.unwrap_or(
                            if config.archive.daily_source == "archive" {
                                ReportSource::Archive
                            } else {
                                ReportSource::Scan
                            },
                        ),
                        start: target,
                        end: target,
                        label: target.to_string(),
                    },
                    args.input.clone().unwrap_or_default(),
                    !args.no_save,
                    args.dry_run,
                    args.scheduled,
                    reason,
                )
            }
            Self::Weekly(args) => (
                Period::weekly(today, args.period.as_deref(), args.source)?,
                args.input.clone().unwrap_or_default(),
                !args.no_save,
                false,
                false,
                None,
            ),
            Self::Monthly(args) => (
                Period::monthly(today, args.period.as_deref(), args.source)?,
                args.input.clone().unwrap_or_default(),
                !args.no_save,
                false,
                false,
                None,
            ),
            _ => return Err(ReportError::new("COMMAND_INVALID", "该命令不生成报告")),
        };
        let context = RunContext::new(run_id, now, period.start, fingerprint)?;
        Ok(ReportRequest {
            period,
            context,
            input,
            save,
            dry_run,
            scheduled,
            regeneration_reason,
        })
    }
}
