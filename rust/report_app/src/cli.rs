use crate::{
    config::AppConfig,
    types::{self, Period, ReportMode, ReportRequest, ReportSource, RunContext},
    ReportError,
};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(version, about = "AI Daily Report — 个人日报与历史证据")]
pub struct Cli {
    #[arg(long, global = true, help = "配置目录；默认使用程序旁的 config")]
    pub config_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
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
    /// Retired generation workflows remain recognizable for old scripts.
    #[command(name="weekly", aliases=["monthly","backfill","coverage","ocr"], hide=true)]
    Retired(RetiredArgs),
    /// 查看历史报告
    List,
    #[command(about = "只读检索日报、周报和月报正文")]
    Search(SearchArgs),
    /// 查看报告运行及调用状态
    Runs {
        #[arg(long, value_parser = types::parse_date)]
        target_date: Option<NaiveDate>,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },
    /// 从数据库恢复原正文，不调用模型
    RepairOutput {
        #[arg(long, value_parser = types::parse_date, required_unless_present="run_id", conflicts_with="run_id")]
        target_date: Option<NaiveDate>,
        #[arg(
            long,
            required_unless_present = "target_date",
            conflicts_with = "target_date"
        )]
        run_id: Option<String>,
    },
    /// 检查配置、模板、数据库及解析进程
    #[command(alias = "check-config")]
    Doctor {
        #[arg(long)]
        strict: bool,
        #[arg(long, help = "发送计入审计与预算的合成模型请求")]
        check_model: bool,
    },
    /// 只读检查任务、采集、日报发布、磁盘与数据库状态
    Health(HealthArgs),
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
pub struct RetiredArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SearchKind {
    All,
    Daily,
    Weekly,
    Monthly,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    #[arg(
        long,
        required_unless_present = "reindex",
        conflicts_with = "reindex",
        help = "检索关键词"
    )]
    pub query: Option<String>,
    #[arg(long, value_parser = types::parse_date, help = "日报起始日期（含当天）；排除周月报")]
    pub from: Option<NaiveDate>,
    #[arg(long, value_parser = types::parse_date, help = "日报结束日期（含当天）；排除周月报")]
    pub to: Option<NaiveDate>,
    #[arg(
        long,
        value_enum,
        default_value = "all",
        help = "报告类型：全部、日报、周报或月报"
    )]
    pub kind: SearchKind,
    #[arg(long, help = "来源标签，如 S001；大小写不敏感")]
    pub source: Option<String>,
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=500), help = "最多返回的片段数（1 至 500）")]
    pub limit: u32,
    #[arg(
        long,
        value_enum,
        default_value = "text",
        help = "输出格式：文本或 JSON"
    )]
    pub format: crate::archive::cli::Format,
    #[arg(long, help = "强制重建独立索引并输出摘要，不执行检索")]
    pub reindex: bool,
}

#[derive(Debug, Args)]
pub struct HealthArgs {
    #[arg(long, value_parser = types::parse_date)]
    pub target_date: Option<NaiveDate>,
    #[arg(long, value_enum, default_value = "json")]
    pub format: crate::archive::cli::Format,
    #[arg(long, help = "写入健康文件与调度日志；供健康任务使用")]
    pub scheduled: bool,
    #[arg(long, help = "另外保存 UTF-8 JSON，供通知程序读取")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    Plan {
        #[arg(long,default_value="daily",value_parser=["daily","health"])]
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
        let (period, input, save, dry_run, scheduled, regeneration_reason) = match self {
            Self::Daily(args) => {
                if config.report.profile != crate::config::ReportProfile::Personal {
                    return Err(ReportError::new("FEATURE_RETIRED", "旧通用日报生成已退役，请显式使用 report.profile: personal；历史报告仍可查询和恢复"));
                }
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
                        source: args.source.unwrap_or(ReportSource::Scan),
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
