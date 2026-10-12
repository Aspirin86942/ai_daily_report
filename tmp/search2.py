import sqlite3, re

con = sqlite3.connect('data/db/scan_index_v3.sqlite3')
con.row_factory = sqlite3.Row

kws = ['部门直属负责人', '分支条件', '财务管理中心负责人(OA)', '财经中心-部门直属负责人', '内部资金调拨', '购买理财产品']
print('===== parse_cache content search')
for k in kws:
    print('---', k)
    for r in con.execute("SELECT file_identity, content FROM parse_cache WHERE content LIKE ? LIMIT 5", (f'%{k}%',)):
        c = r['content']
        print('  >>', r['file_identity'].split('\\')[-1])
        for m in re.finditer(re.escape(k), c):
            s = max(0, m.start()-300); e = min(len(c), m.end()+500)
            print('     ...', c[s:e].replace('\n', ' | '))
            print()
print()
print('===== context_artifacts 4,5 full excerpt around 财务管理中心负责人')
for aid in (4, 5):
    for r in con.execute("SELECT final_context FROM context_artifacts WHERE artifact_id = ?", (aid,)):
        c = r['final_context']
        for m in re.finditer('财务管理中心负责人', c):
            s = max(0, m.start()-800); e = min(len(c), m.end()+1200)
            print(f'--- artifact {aid}:')
            print(c[s:e])
            print('-----')
con.close()
