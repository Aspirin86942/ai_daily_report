"""Migrate daily_reports table from old schema to v5 simplified schema.

Old columns: date, summary, achievements_json, risks_json, plans_json,
              yesterday_review, raw_json, created_at, updated_at
New columns: date, completed_work, work_summary, next_plan,
             raw_json, created_at, updated_at
"""

import json
import sqlite3
import sys
from pathlib import Path


def migrate(db_path: str) -> None:
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row

    # Check if migration is needed
    cols = {r[1] for r in conn.execute("PRAGMA table_info(daily_reports)").fetchall()}
    if "completed_work" in cols:
        print("Already migrated, skipping.")
        return

    print(f"Migrating {db_path}...")

    # 1. Rename old table and drop old index
    conn.execute("ALTER TABLE daily_reports RENAME TO daily_reports_old")
    conn.execute("DROP INDEX IF EXISTS idx_daily_reports_date")
    conn.commit()

    # 2. Create new table
    conn.executescript("""
        CREATE TABLE daily_reports (
            date TEXT PRIMARY KEY,
            completed_work TEXT NOT NULL,
            work_summary TEXT NOT NULL,
            next_plan TEXT NOT NULL,
            raw_json TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX idx_daily_reports_date ON daily_reports(date);
    """)
    conn.commit()

    # 3. Migrate rows
    old_rows = conn.execute(
        "SELECT * FROM daily_reports_old ORDER BY date"
    ).fetchall()

    for row in old_rows:
        # Parse old structured data
        achievements = json.loads(row["achievements_json"] or "[]")
        summary = row["summary"] or ""
        plans = json.loads(row["plans_json"] or "[]")
        yesterday_review = row["yesterday_review"] or ""

        # Build completed_work from achievements
        achievement_texts = []
        for a in achievements:
            cat = a.get("category", "")
            content = a.get("content", "")
            quantitative = a.get("quantitative", "")
            parts = [p for p in [cat, content, quantitative] if p]
            achievement_texts.append("：".join(parts) if parts else "")
        completed_work = "；".join(t for t in achievement_texts if t)

        # Build next_plan from plans
        if isinstance(plans, list):
            next_plan = "；".join(p for p in plans if isinstance(p, str) and p)
        else:
            next_plan = ""

        # Build new raw_json
        new_raw = {
            "date": row["date"],
            "completed_work": completed_work,
            "work_summary": summary,
            "next_plan": next_plan,
        }
        raw_json = json.dumps(new_raw, ensure_ascii=False)

        conn.execute(
            """INSERT INTO daily_reports
               (date, completed_work, work_summary, next_plan, raw_json, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)""",
            (row["date"], completed_work, summary, next_plan, raw_json,
             row["created_at"], row["updated_at"]),
        )

    conn.commit()
    print(f"Migrated {len(old_rows)} rows.")

    # 4. Drop old table
    conn.execute("DROP TABLE daily_reports_old")
    conn.commit()

    conn.close()
    print("Done.")


if __name__ == "__main__":
    db = sys.argv[1] if len(sys.argv) > 1 else "data/db/reports.sqlite3"
    if not Path(db).exists():
        print(f"Database not found: {db}")
        sys.exit(1)
    migrate(db)
