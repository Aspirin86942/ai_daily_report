import openpyxl, glob, os

files = [
    r'd:\01- 工作\2026年\32- 流程梳理\刘智流程统计\2025_07_01-2025_12_31-全部收到流程.xls',
    r'd:\01- 工作\2026年\32- 流程梳理\刘智流程统计\2026_01_01-2026_06_30-全部收到流程.xls',
    r'd:\01- 工作\2026年\32- 流程梳理\刘智流程统计\刘智流程统计.xlsx',
]
kws = ['财经', '财务管理', '直属负责人', '负责人']
for f in files:
    print('='*100)
    print('FILE:', os.path.basename(f))
    try:
        wb = openpyxl.load_workbook(f, data_only=True, read_only=True)
    except Exception as e:
        print('  open failed:', e)
        continue
    for ws in wb.worksheets:
        hit = 0
        for row in ws.iter_rows(values_only=True):
            for v in row:
                if v is None: continue
                s = str(v)
                for k in kws:
                    if k in s:
                        print(f'  [{ws.title}]', s[:300].replace('\n', ' '))
                        hit += 1
                        break
            if hit > 60: break
    wb.close()
