import sqlite3, re

con = sqlite3.connect('data/db/scan_index_v3.sqlite3')
con.row_factory = sqlite3.Row

targets = [
    '查询-【集团】付款申请单 (2).xlsx',
    '流程统计-总经理.xlsx',
    '刘智流程统计.xlsx',
]
for t in targets:
    print('='*100)
    print('FILE:', t)
    print('='*100)
    for r in con.execute("SELECT content, entry_size_bytes, cached_at_ms FROM parse_cache WHERE file_identity LIKE ? LIMIT 1", (f'%{t}%',)):
        print(r['content'])
    print()

print('='*100)
print('CONTEXT ARTIFACTS 4,5 final_context (excerpts around keywords)')
print('='*100)
for aid in (4, 5):
    for r in con.execute("SELECT final_context FROM context_artifacts WHERE artifact_id = ?", (aid,)):
        c = r['final_context']
        for kw in ['付款申请单', '财务管理中心负责人', '财经中心']:
            for m in re.finditer(re.escape(kw), c):
                s = max(0, m.start()-400); e = min(len(c), m.end()+600)
                print(f'--- artifact {aid} around "{kw}":')
                print(c[s:e])
                print()
con.close()
