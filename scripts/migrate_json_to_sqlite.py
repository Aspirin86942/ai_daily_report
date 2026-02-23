"""Migrate JSON report database into SQLite."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from src.core.config import config
from src.services.json_to_sqlite_migrator import JSONToSQLiteMigrator


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Migrate JSON daily/weekly/monthly report files to SQLite."
    )
    parser.add_argument(
        "--json-db-dir",
        type=Path,
        default=config.db_dir,
        help="JSON db directory (default: config.paths.db_dir).",
    )
    parser.add_argument(
        "--sqlite-db-path",
        type=Path,
        default=config.db_dir / "reports.sqlite3",
        help="Target SQLite database path.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Validate and count files without writing SQLite data.",
    )
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    migrator = JSONToSQLiteMigrator(
        json_db_dir=args.json_db_dir,
        sqlite_db_path=args.sqlite_db_path,
    )
    stats = migrator.migrate(dry_run=args.dry_run)

    mode = "DRY RUN" if args.dry_run else "MIGRATION"
    print(f"[{mode}] JSON dir: {args.json_db_dir}")
    print(f"[{mode}] SQLite DB: {args.sqlite_db_path}")
    print(
        "daily   found={0} migrated={1} failed={2}".format(
            stats.daily_found, stats.daily_migrated, stats.daily_failed
        )
    )
    print(
        "weekly  found={0} migrated={1} failed={2}".format(
            stats.weekly_found, stats.weekly_migrated, stats.weekly_failed
        )
    )
    print(
        "monthly found={0} migrated={1} failed={2}".format(
            stats.monthly_found, stats.monthly_migrated, stats.monthly_failed
        )
    )
    print(
        "total   found={0} migrated={1} failed={2}".format(
            stats.total_found, stats.total_migrated, stats.total_failed
        )
    )

    return 1 if stats.total_failed > 0 else 0


if __name__ == "__main__":
    raise SystemExit(main())
