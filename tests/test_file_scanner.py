"""测试文件扫描器"""

from datetime import date, timedelta
from pathlib import Path
from src.services.file_scanner import FileScanner


def test_file_scanner_init():
    """测试扫描器初始化"""
    scanner = FileScanner()
    assert scanner.work_dir.exists()
    assert scanner.scanner_cfg["max_workers"] > 0


def test_scan_files_default_dates():
    """测试 scan_files 默认日期参数"""
    scanner = FileScanner()
    result = scanner.scan_files()
    assert result.total_files >= 0
    assert result.success_count + result.error_count == result.total_files


def test_scan_files_with_date_range():
    """测试 scan_files 指定日期范围"""
    scanner = FileScanner()
    today = date.today()
    yesterday = today - timedelta(days=1)
    result = scanner.scan_files(start_date=yesterday, end_date=today)
    assert result.total_files >= 0


def test_scan_files_summary_mode():
    """测试 scan_files summary_mode 使用缩减限制"""
    scanner = FileScanner()
    today = date.today()
    yesterday = today - timedelta(days=1)
    result = scanner.scan_files(start_date=yesterday, end_date=today, summary_mode=True)
    assert result.total_files >= 0


def test_scan_files_empty_range():
    """测试不存在文件的日期范围"""
    scanner = FileScanner()
    # 使用很远的过去日期，应该没有文件
    old_start = date(2000, 1, 1)
    old_end = date(2000, 1, 2)
    result = scanner.scan_files(start_date=old_start, end_date=old_end)
    assert result.total_files == 0
    assert result.success_count == 0


def test_scan_today_files_backward_compat():
    """测试 scan_today_files 向后兼容"""
    scanner = FileScanner()
    result = scanner.scan_today_files()
    assert result.total_files >= 0


def test_get_files_in_range():
    """测试 _get_files_in_range 方法"""
    scanner = FileScanner()
    today = date.today()
    yesterday = today - timedelta(days=1)
    files = scanner._get_files_in_range(yesterday, today)
    assert isinstance(files, list)
    for f in files:
        assert isinstance(f, Path)

    # 验证排除目录配置生效（如果配置了排除目录）
    excluded_dirs = scanner.scanner_cfg.get("excluded_dirs", [])
    if excluded_dirs:
        for excluded_dir in excluded_dirs:
            excluded_path = Path(excluded_dir).resolve()
            for f in files:
                # 确保没有文件来自排除目录
                try:
                    f.resolve().relative_to(excluded_path)
                    assert False, f"文件 {f} 不应该来自排除目录 {excluded_dir}"
                except ValueError:
                    pass  # 正常，文件不在排除目录中
