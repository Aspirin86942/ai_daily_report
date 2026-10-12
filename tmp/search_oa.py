import sqlite3

con = sqlite3.connect('data/db/scan_index_v3.sqlite3')
con.row_factory = sqlite3.Row
kw = ['付款申请单', '财务管理中心负责人', '财经中心']

# 1) parse_cache content search
print('==== parse_cache LIKE search')
rows = list(con.execute("SELECT file_identity, parser_backend, entry_size_bytes, cached_at_ms FROM parse_cache"))
print('parse_cache entries:', len(rows))
for r in rows:
    print('  ', r['file_identity'], '|', r['parser_backend'], '|', r['entry_size_bytes'])

# search content
for k in kw:
    print('--- content LIKE', k)
    for r in con.execute("SELECT file_identity, entry_size_bytes FROM parse_cache WHERE content LIKE ? LIMIT 20", (f'%{k}%',)):
        print('  ', r['file_identity'], r['entry_size_bytes'])

# 2) context_artifacts
print('==== context_artifacts schema')
for row in con.execute("PRAGMA table_info(context_artifacts)"):
    print(' ', dict(row))
print('==== context_artifacts count')
for r in con.execute("SELECT COUNT(*) c FROM context_artifacts"):
    print(r['c'])
for k in kw:
    print('--- artifact LIKE', k)
    for r in con.execute("SELECT artifact_id FROM context_artifacts WHERE final_context LIKE ? LIMIT 20", (f'%{k}%',)):
        print('  artifact_id:', r['artifact_id'])
con.close()
