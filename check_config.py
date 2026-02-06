"""配置验证脚本"""
import sys
import os
from pathlib import Path

# 设置控制台编码为 UTF-8
if sys.platform == 'win32':
    os.system('chcp 65001 > nul')


def check_config():
    """检查配置文件"""
    print("===== 配置检查 =====\n")

    errors = []
    warnings = []

    # 检查配置文件
    config_dir = Path("config")
    settings_file = config_dir / "settings.toml"
    secrets_file = config_dir / ".secrets.toml"

    if not settings_file.exists():
        errors.append(f"缺少配置文件: {settings_file}")
    else:
        print(f"[OK] 找到配置文件: {settings_file}")

    if not secrets_file.exists():
        errors.append(f"缺少敏感配置文件: {secrets_file}")
    else:
        print(f"[OK] 找到敏感配置文件: {secrets_file}")

    # 检查模板文件
    templates_dir = Path("templates")
    template_files = [
        "system_prompt.md",
        "report_template.md",
        "weekly_prompt.md",
        "monthly_prompt.md",
        "weekly_template.md",
        "monthly_template.md",
    ]

    for tpl_name in template_files:
        tpl_path = templates_dir / tpl_name
        if not tpl_path.exists():
            errors.append(f"缺少模板文件: {tpl_path}")
        else:
            print(f"[OK] 找到模板文件: {tpl_path}")

    # 检查目录
    data_dir = Path("data")
    if not data_dir.exists():
        warnings.append(f"数据目录不存在: {data_dir} (将自动创建)")
    else:
        print(f"[OK] 找到数据目录: {data_dir}")

    # 尝试加载配置
    try:
        from src.core.config import config
        print(f"\n[OK] 配置加载成功")
        print(f"  - 工作目录: {config.work_dir}")
        print(f"  - LLM 模型: {config.llm_config['model_id']}")
        print(f"  - 最大并发: {config.scanner_config['max_workers']}")

        # 检查 API Key
        api_key = config.api_key
        if not api_key or api_key.startswith("${"):
            errors.append("未配置 GOOGLE_API_KEY")
        else:
            print(f"  - API Key: {api_key[:10]}...")

    except Exception as e:
        errors.append(f"配置加载失败: {e}")

    # 检查依赖
    print("\n===== 依赖检查 =====\n")
    required_packages = [
        'google.genai',
        'pydantic',
        'dynaconf',
        'rich',
        'pandas',
        'openpyxl',
        'pptx',
        'pdfplumber',
        'jinja2'
    ]

    for package in required_packages:
        try:
            __import__(package.replace('-', '_'))
            print(f"[OK] {package}")
        except ImportError:
            errors.append(f"缺少依赖包: {package}")

    # 输出结果
    print("\n===== 检查结果 =====\n")

    if warnings:
        print("警告:")
        for w in warnings:
            print(f"  [!] {w}")
        print()

    if errors:
        print("错误:")
        for e in errors:
            print(f"  [X] {e}")
        print("\n配置检查失败，请修复上述问题")
        return False
    else:
        print("[OK] 所有检查通过，可以正常使用")
        return True


if __name__ == '__main__':
    success = check_config()
    sys.exit(0 if success else 1)
