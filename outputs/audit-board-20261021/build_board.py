from __future__ import annotations

import copy
import hashlib
import html
import json
import re
from collections import Counter
from pathlib import Path


REPO = Path(__file__).resolve().parents[2]
SOURCE = Path('C:/Users/lcf/Desktop/审计项目看板-数据.json')
WORK = Path('D:/01- 工作/2026年')
TEMPLATE = WORK / '62- 工作待办整理/审计项目看板.html'
DEST = Path('C:/Users/lcf/Desktop/审计项目梳理_2026-10-21')
AS_OF = '2026-10-21'


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


source_hash = sha(SOURCE)
template_hash = sha(TEMPLATE)
original = json.loads(SOURCE.read_text(encoding='utf-8-sig'))
board = copy.deepcopy(original)
board['cutoff'] = AS_OF
names = list(dict.fromkeys(t['p'] for t in board['tasks']))
assert len(names) == 18 and len(board['tasks']) == 42


def daily(date: str, quote: str) -> tuple[str, str]:
    return str(REPO / 'data/reports' / date[:7] / (date + '.md')), quote


def work(rel: str, quote: str) -> tuple[str, str]:
    return str(WORK / rel), quote


# Status belongs to the user's current ledger. Source files substantiate the
# descriptions and milestones; an older unresolved item does not overturn it.
project_specs = [
    dict(
        summary='跟踪 GB 审查问题整改，并将成本、工时、外协仓及供应商管理问题纳入整改结论。',
        basis='原台账', folders=['05- GB成本分析', '08- GB问题跟进'],
        next='结项状态与原定日期沿用原台账；需要对外展示时，补录最终整改结论及验收记录的位置。',
        refs=[],
    ),
    dict(
        summary='围绕缺陷认定、高风险领域和同行对标，完成年度内部控制评价材料与报告编制。',
        basis='原台账及日报', folders=['15- 同行业内控评价统计', '21- 制度穿行'],
        next='保留年度内控评价结项口径，报告定稿、底稿及审批记录按同一年度归档。',
        refs=[daily('2026-04-27', '纳入评价的主要业务和事项')],
    ),
    dict(
        summary='跟进 ERP/PLM 账号、离职账户和系统表单整改；EAM 后续流程优化归入独立项目。',
        basis='原台账及日报', folders=['12- 流程整改跟踪', '13- 账号超配跟踪', '34- 账号采购明细'],
        next='原整改专项保留台账完成状态；EAM 的后续方案和验收在 EAM 固定资产管理流程优化专项跟进。',
        refs=[daily('2026-08-06', '财务中心回复称不存在大量闲置')],
    ),
    dict(
        summary='完成中基首次财务资料检查，关注收入成本、科目余额、序时账及关联交易。',
        basis='原台账', folders=['14- 中基项目'],
        next='首次尽调与后续关联方现场调研分别保留，按各自底稿和报告归档。', refs=[],
    ),
    dict(
        summary='围绕 DIG、道本金属及武汉锅炉的往来情况开展现场调研并形成报告。',
        basis='原台账', folders=['14- 中基项目'],
        next='保留独立项目名称，以《中基及关联方调研报告》及现场资料对应结项记录。', refs=[],
    ),
    dict(
        summary='检查基建、信息披露及内控执行材料，完成现场核查应对资料整理。',
        basis='原台账', folders=['21- 制度穿行'],
        next='保留已完成状态；应对资料目录、提交版本及归档位置按原台账补齐。', refs=[],
    ),
    dict(
        summary='完成 OA 与 ERP 报废数据比对、业务访谈及流程诊断，形成表单和批量导入优化方案。',
        basis='原台账', folders=['25- 报废流程梳理及优化'],
        next='原台账完成口径包含方案汇报与交 IT 开发；系统上线及业务验收应有对应记录。', refs=[],
    ),
    dict(
        summary='完成如本科技尽调程序设计、访谈资料复核及报告归档。',
        basis='原台账', folders=['28- 如本尽调'],
        next='保留原台账结项状态，按方案、访谈清单、底稿、报告整理成果目录。', refs=[],
    ),
    dict(
        summary='完成呆滞料筛选、四方交易及单据穿透分析，形成流程优化方案；保留后续配置核对事项。',
        basis='原台账及日报', folders=['26- 呆滞料流程梳理及优化', '58- 待办清单'],
        next='10 月 10 日日报仍记载“处理建议选项和说明是否需配置”未完成，需补最新处理结论。',
        refs=[daily('2026-10-10', '呆滞料处理的数据导出功能')],
    ),
    dict(
        summary='围绕倍可的关联方、财务交易、收入毛利及人员管理完成尽调资料分析，并形成问题汇总。',
        basis='原台账及日报', folders=['30- 倍可机器人'],
        next='完成状态沿用原台账。可定位成果为问题汇总和分析底稿；正式尽调报告、签收及实际起止日期待补。',
        keys=['资料清单', '股权与关联方', '收入及毛利', '关联采购', '人员与潜在负债', '问题汇总'],
        refs=[daily('2026-07-14', '共24个去重主体'), daily('2026-07-17', '工资潜在负债复核底稿'), daily('2026-07-27', '文件“问题汇总表”记载')],
    ),
    dict(
        summary='已开展制度与披露资料梳理，并形成两个募集批次的公告台账；当前重点是补齐实际资金证据。',
        basis='日报及成果说明', folders=['40- 集团募集资金专项'],
        next='取得银行流水或函证、募集资金及内部往来底账、置换与付款凭证等，核对批准额、实际发生额和期末余额。',
        keys=['募集批次与项目', '专户及监管协议', '资金使用', '现金管理', '借款与置换', '披露差异与补证'],
        refs=[daily('2026-08-19', '两份文件对改变募集资金用途'), work('40- 集团募集资金专项/01- 审计计划/募集资金台账/募集资金台账_使用说明.md', '本台账依据28份公告')],
    ),
    dict(
        summary='整理 OA 组织角色与审批条件，完成七类流程的金额分档和新旧方案对比，并延伸至业务线与职能线职责讨论。',
        basis='原台账及日报', folders=['52- 权限整理表', '53- OA三级审批优化方案', '55- CRMorOA谢总方案', '58- 待办清单', '59- 流程看板需求设计稿'],
        next='完成状态沿用原台账。10 月 10 日费用报销配置、三级审批核对仍有记录缺口，10 月 15 日部分 OA/CRM 职责仍在讨论，需补最新确认与验收依据。',
        keys=['七类 OA 流程', '金额分档', '新旧方案对比', '组织角色映射', '业务线与职能线职责', '配置验收'],
        refs=[daily('2026-09-28', '覆盖付款申请单'), daily('2026-10-10', '第22项集团三级审批'), daily('2026-10-15', '业务与职能审批、知会矩阵（讨论稿）')],
    ),
    dict(
        summary='完成采购订单和付款申请的单据汇总、金额分布及审批档位比较，覆盖多公司及波锋、波斩、精密补充分析。',
        basis='原台账及日报', folders=['31- 采购订单金额梳理', '43- 波峰波斩采购订单及付款申请分析', '54- ERP精密采购订单'],
        next='完成状态沿用原台账；审批分析和规则展示成果已有记录，最终业务批准及 ERP 实际生效范围待补充。',
        keys=['采购订单', '付款申请', '单据去重与异常排除', '金额分布', '审批金额档位', '公司差异'],
        refs=[daily('2026-07-15', '源数据为采购订单明细73952行'), daily('2026-09-15', '四行交付名称'), daily('2026-09-23', '未修改审批规则')],
    ),
    dict(
        summary='销售 BOM 一期已精简审批链和表单字段，材料明确记载于 2026 年 9 月 3 日上线。',
        basis='原台账、日报及成果说明', folders=['39- bom新建修改表流程优化', '45- bom新建修改表流程优化2.0', '58- 待办清单'],
        next='一期保留已完成状态；审批意见反写在 10 月 10 日仍缺新单据验证，需要补验收样本。二期继续处理分层分级。',
        keys=['现状及节点分析', '无效节点清理', '冗余字段清理', '产品/计划/财务职责', '9月3日上线', '审批意见反写验证'],
        refs=[daily('2026-08-28', '审批链条过长'), daily('2026-09-17', '于2026-09-03上线'), daily('2026-10-10', '销售BOM修复审批意见反写'), work('45- bom新建修改表流程优化2.0/销售BOM流程优化v3.0_汇报大纲.md', '已于2026年9月3日上线')],
    ),
    dict(
        summary='已梳理品类策略与战略备货的定位、金额及 OA—ERP 衔接问题，流程闭环与开发排期待落实。',
        basis='原台账及日报', folders=['36- 品类策略审批单', '41- 柏楚品类策略审批单流程优化', '58- 待办清单'],
        next='明确策略更新、战略备货和研发备料的适用边界，落实 ERP 关联与系统交期（LT）取数范围、排期及验收。',
        keys=['品类策略定位', '战略库存金额', '审批与采购申请衔接', 'OA—ERP 关联', '审批后执行复核', '交期取数'],
        refs=[daily('2026-08-21', '金额明细”共38行'), daily('2026-08-25', '无关联编号或表头备注'), daily('2026-10-10', 'ERP对接及系统LT取数排期')],
    ),
    dict(
        summary='已完成供应商承认与定点流程访谈、字段分析及方案整理，正在细化分级、准入和审批责任。',
        basis='原台账及日报', folders=['42- 智慧工厂业务线定点审批流程优化'],
        next='确认供应商分级与红线、豁免规则、承认和定点的前后关系、部门责任及上线验收安排。',
        keys=['供应商分级', '准入标准与红线', '承认/定点衔接', '评审职责', '字段清理', '例外与复审'],
        refs=[daily('2026-09-07', '召开供应商准入及定点流程优化启动会'), daily('2026-09-08', '纯僵尸字段3个'), daily('2026-09-09', '待决策事项重组为A/B/C/D四组共18项'), daily('2026-10-08', '判断矩阵以')],
    ),
    dict(
        summary='已形成固定资产全生命周期流程图和会议修订意见，当前重点是统一节点口径并落实配置。',
        basis='原台账、日报及会议纪要', folders=['37- EAM系统优化会议'],
        next='统一流程图与纪要，确认处置金额边界、跨部门合单、岗位映射和遗失赔偿；新建实物处置单及系统配置待验收。',
        keys=['资产验收', '借用与归还', '转移与离职交接', '处置分级审批', '报废实物处理', '硬盘管理'],
        refs=[daily('2026-10-12', '处置审批档位'), work('37- EAM系统优化会议/eam优化会议二次会议1012/固定资产管理流程优化会议纪要.md', '尚需业务确认的事项单独列示')],
    ),
    dict(
        summary='在一期成果基础上，设计销售 BOM 常规、重要、重大分层分级，细化判定条件与审批责任。',
        basis='原台账、日报及成果说明', folders=['45- bom新建修改表流程优化2.0'],
        next='确认各层级判定条件、对应审批岗位、规则负责人和试行范围；明确 ERP 自动写入及产品管理平台衔接边界。',
        keys=['常规/重要/重大', '新建与修改分类', '关键物料与产品搭配', '审批岗位映射', '规则负责人与试行范围', 'ERP/平台衔接'],
        refs=[daily('2026-09-17', '阶段二“常规/重要/重大”三级审批方案'), daily('2026-09-29', '属方案方向表述'), work('45- bom新建修改表流程优化2.0/销售BOM流程优化v3.0_汇报大纲.md', '本次需确认：各层级的判定条件')],
    ),
]


old_outputs = [
    '原台账载明：GB 审查问题整改跟踪记录及系统数据复核记录；具体文件待定位。',
    '原台账载明：问题整改跟踪结论及整改策略；具体结项文件待定位。',
    '原台账载明：同行业内控评价对标资料、缺陷认定标准与高风险领域整理。',
    '原台账载明：内控评价查询表单、内控底稿及同行报告比对资料。',
    '原台账载明：《2025年度内部控制评价报告》；最终定稿及审批记录待定位。',
    '原台账载明：ERP/PLM 账号、离职账户和系统表单问题清单。',
    '原台账载明：系统表单整改复核、账号闲置率及离职账户禁用核对记录。',
    '原台账载明：信息系统整改闭环跟踪材料；具体结项文件待定位。',
    '原台账载明：中基审计报告、财务报表、收入成本、科目余额表及序时账核查底稿。',
    '原台账载明：中基财务状况初步分析结论及关联交易、异常事项整理。',
    '原台账载明：DIG、道本金属、武汉锅炉往来及现场调研资料。',
    '原台账载明：《中基及关联方调研报告》。',
    '原台账载明：部门访谈、基建/信息披露/内控执行检查及核查关注事项。',
    '原台账载明：应对证监局检查提交资料及归档目录。',
    '原台账载明：OA 报废申请与 ERP 报废其他出库的数量、金额及单据差异分析。',
    '原台账载明：各业务单元报废流程访谈记录与需求差异整理。',
    '原台账载明：《存货报废流程梳理&诊断》、表单表体重构方案及批量导入/字段优化材料。',
    '原台账载明：尽调程序库母版及五大模块尽调重点。',
    '原台账载明：尽调访谈清单、资料复核记录及计算底稿。',
    '原台账载明：如本尽调报告及方案、访谈清单、底稿归档资料。',
    '原台账载明：呆滞预警、预呆滞分析与可售卖物料筛选访谈材料。',
    '原台账载明：四方交易访谈及采购订单、销售出库单、应收单穿透分析。',
    '原台账载明：呆滞料售卖流程诊断 PPT、标准化流程及 IT 开发交接材料。',
]
for task, output in zip(board['tasks'][:23], old_outputs, strict=True):
    task['out'] = output
    task['iss'] = '原台账未单列未决事项；完成状态和原定日期沿用原台账。'
board['tasks'][7]['iss'] = '本专项完成状态沿用原台账；EAM 后续流程优化由“2026年度EAM固定资产管理流程优化专项”继续跟进。'
board['tasks'][16]['iss'] = '原台账成果包括方案宣贯及交 IT 开发；系统上线与业务验收时间需由对应记录确认。'
board['tasks'][22]['iss'] = '10月10日日报记载：数据导出及第一层级负责人取人已修改，处理建议选项和说明是否需配置仍未完成；需补最新结论。完成状态沿用当前原台账。'

# New entries use empty date strings supported by the companion board.
# No material-modification date is promoted to an actual project start/end.
new_rows = [
    ('前期方案讨论', '整理倍可尽调资料清单和核查范围，围绕主体股权、关联方、财务、经营及人员事项准备资料；形成关联方关系图与明细。', '倍可机器人尽调资料清单-0714(2).xlsx；关联方关系图及关联关系明细。', '区分直接关联、人员穿透和历史关系，关联关系按核查时点识别。'),
    ('访谈与资料检查', '核对财务账表、发票、销售合同及发货资料，分析收入毛利、关联采购、借款、社保公积金与工资潜在负债，并整理人员及组织管理问题。', '毛利分析2025年-2026年_按销售订单归集.xlsx；序时账-简单版本.xlsx；工资潜在负债复核及交易核查资料。', '关注关联采购交易环节与实物流、收入凭证对应关系及岗位职责；材料中的风险判断需结合底稿复核。'),
    ('总结汇报', '汇总倍可尽调发现的关联交易、盈利质量、人员组织及内部控制问题，形成问题清单和支撑底稿。', '倍可机器人尽调问题汇总.xlsx；相关交易及财务分析底稿。', '正式尽调报告、结项签收和实际完成日期待补；原台账重复的“访谈与资料检查”阶段调整为“总结汇报”。'),
    ('审计准备与资料梳理', '梳理募集资金制度与披露资料，围绕两个募集批次建立公告台账，整理项目状态、专户、现金管理、特殊用款及披露差异；实际资金核查继续推进。', '募集资金公告台账_2024末至2026半年.xlsx；募集资金生命周期总表_简版.xlsx；募集资金台账_使用说明.md。', '实际银行流水、账簿和凭证仍需补证；批准上限、实际发生额与余额分别核对；关注期后事项及披露差异。'),
    ('前期方案讨论', '结合集团组织角色及业务职责，梳理 OA 三级审批现状，确定付款、差旅、网购付款、费用报销、采购申请、销售订单和合同七类流程的优化范围。', '权限整理表；OA三级审批优化方案；各流程现状审批条件与岗位整理。', '一级/二级组织角色、业务线及职能线的参与职责需按业务口径对应，特殊公司和流程单独处理。'),
    ('访谈与资料检查', '按各流程统计期间汇总单据和金额，核对审批节点及触发条件，对比新旧金额档位、审批链及单量变化，并整理业务沟通意见。', '七类 OA 流程金额分布与档位模拟；流程&权限汇总表-谢总_意见已填写.xlsx；OA与CRM审批流程优化会议纪要。', '各流程、公司及统计窗口分别计算；单据量模拟、审批金额覆盖和系统实际生效状态分别记录。'),
    ('总结汇报', '形成 OA 三级审批优化方案和新旧审批条件汇总，整理市场服务中心 OA/CRM 职责及业务线、职能线审批矩阵，汇总配置核对事项。', 'OA三级审批优化方案汇总及七类流程方案；谢总业务与职能组织矩阵；李乐乐待办_逐项核对表.xlsx。', '原台账标为已完成；10月10日费用报销配置与三级审批核对仍有缺口，10月15日部分职责仍在讨论，需补最新业务确认及验收结论。'),
    ('前期方案讨论', '整理各公司 ERP 采购订单与付款申请审批规则，明确按单据汇总、异常单据排除、金额区间及审批角色的分析口径。', '采购订单、付款审批优化.xlsx；采购订单与付款申请分析说明。', '各批分析的重新审核、零金额及非正金额排除规则存在差异，应随数据范围保留。'),
    ('访谈与资料检查', '分析采购订单、付款申请的单量和金额分布，比较原档位、按单量比例分档及人工调整方案，并补充波锋、波斩和精密采购订单分析。', '采购订单金额分布—终版0715.xlsx；波锋/波斩四类金额分布分析；采购订单金额分布_精密机电.xlsx。', '单据审批量与金额覆盖同时比较，明确金额等于阈值时的归属及各公司的差异。'),
    ('总结汇报', '整理各公司采购订单及付款申请审批档位方案、对比结果和规则展示材料，形成可复核的分析交付。', 'execl交付.xlsx；采购订单及付款申请审批档位方案；审批规则展示材料。', '原台账标为已完成；分析交付及规则展示有材料支撑，最终业务批准和 ERP 实际生效范围需补记录。'),
    ('前期方案讨论', '明确销售 BOM 新建及修改流程的优化范围，梳理长审批链、失效分支、重复节点和冗余字段，并准备产品、计划及 IT 访谈。', '产品bom分析会议问题.xlsx；判断条件-节点名称.xlsx；产品/IT及计划访谈资料。', '一期聚焦现有流程节点与字段精简；二期承接按业务影响分层分级。'),
    ('访谈与资料检查', '分析销售 BOM 新建、修改及产品属性分布，核对产品、计划、财务各节点职责，形成审批链与表单字段调整清单。', 'BOM新建修改单总结.xlsx；BOM新建修改单分级占比.xlsx；BOM新建修改申请表_产品属性分布.xlsx。', '不同材料使用不同时间窗、单据口径与物料行口径，数量和节点指标需保留各自范围。'),
    ('总结汇报', '形成销售 BOM 一期流程优化材料并完成节点、字段精简；日报和汇报大纲明确记载一期于2026年9月3日上线。', '销售BOM新建修改流程梳理&优化-20260902.pptx；销售BOM流程优化v3.0_汇报大纲.md中的一期上线记录。', '9月3日为明确上线里程碑；阶段起止另待确认。10月10日审批意见反写尚缺新单据验证，需补后续验收样本。'),
    ('前期方案讨论', '梳理品类策略更新、战略库存备货和研发备料单据的定位，识别实际使用场景与重复审批问题。', '品类策略专题会议及采购访谈记录；单据用途与流程现状整理。', '策略更新、战略备货和研发备料的承载单据及适用范围需明确。'),
    ('访谈与资料检查', '核对品类策略审批单和战略备货金额口径，访谈采购部门，梳理 OA 审批至 ERP 采购申请的关联、下单及复核过程。', '柏楚品类策略审批单-金额汇总.xlsx；查询-柏楚品类策略审批单.xlsx；2026年8月25日采购访谈资料。', '现有材料反映 OA 与 ERP 缺少关联编号及审批后执行复核，战略与常规备货金额应分开。'),
    ('总结汇报', '汇总单据定位、表单说明和系统衔接优化事项；附件名称及提交说明已有修改记录，ERP 对接与系统交期取数继续跟进。', '战略备货优化事项；李乐乐待办_逐项核对表.xlsx；相关导入测试资料。', '10月10日记录 ERP 对接与系统LT取数排期未完成；需确认对接范围、开发排期及上线验收。'),
    ('方案设计与业务确认', '开展供应商承认与定点流程访谈和启动会，分析等级、字段及流程衔接，形成优化方案并细化合作关系、采购内容和采购金额判断规则。', '供应商承认与定点审批流程优化方案.html；承认单僵尸字段分析明细.xlsx；智慧工厂业务线供应商流程优化草稿.xlsx。', '分级与准入标准、红线、豁免、复审和部门责任尚需确认；方案形成与表单上线分开验收。'),
    ('方案复核与实施跟踪', '复核固定资产验收、借还、转移、离职交接和处置全流程，整理主管知会、外借分类、处置审批与硬盘管控的会议修订意见。', '固定资产管理流程优化会议纪要_20260813.md；二次会议固定资产管理流程优化会议纪要.md；固定资产整体业务流程图（全流程合并）最新版.pptx。', '会议方向尚待同步流程图及系统配置；处置金额边界、跨部门合单、岗位去重、遗失赔偿和新实物处置单需明确。二次会议实际日期未确认。'),
    ('分层分级方案设计', '承接一期成果，设计销售 BOM 新建与修改事项的分层分级，按常规、重要、重大匹配判定条件及审批职责，并梳理 ERP 人工维护与产品管理平台衔接问题。', '销售BOM流程优化v2.0.pptx；销售BOM流程优化v3.0_汇报大纲.md；分层分级审批_预计整改价值_可编辑.pptx。', '各层级判定条件、审批岗位、规则负责人及试行范围待确认；平台衔接和预期收益保持方案口径。'),
]
assert len(new_rows) == 19
for task, (phase, description, output, issue) in zip(board['tasks'][23:], new_rows, strict=True):
    task.update(ph=phase, d=description, out=output, iss=issue, st='', en='', dy=None)
    task['iss'] += ' 原填10月21日起止日期缺少项目依据，已留空待确认。'

for task in board['tasks']:
    task['pg'] = 1 if task['s'] == '已完成' else None
board['tasks'][26]['c'] = '审计专项'

for name, spec in zip(names, project_specs, strict=True):
    if 'keys' in spec:
        board['keypoints'][name] = spec['keys']

source_cache = {}


def resolve_ref(ref):
    filename, query = ref
    path = Path(filename)
    content = source_cache.setdefault(filename, path.read_text(encoding='utf-8-sig'))
    matches = [n for n, line in enumerate(content.splitlines(), 1) if query in line or query in line.replace('\\', '')]
    if not matches:
        raise ValueError(f'Evidence text not found: {path.name}: {query}')
    return {'path': filename, 'line': matches[0], 'locator': query, 'sha256': sha(path)}


evidence = []
for index, (name, spec) in enumerate(zip(names, project_specs, strict=True), 1):
    rows = [n for n, t in enumerate(board['tasks'], 1) if t['p'] == name]
    evidence.append(dict(
        project=name, task_rows=rows, status='进行中' if any(board['tasks'][r-1]['s']=='进行中' for r in rows) else '已完成',
        summary=spec['summary'], basis=spec['basis'], next=spec['next'],
        directories=[{'path': str(WORK / folder), 'exists': (WORK / folder).is_dir()} for folder in spec['folders']],
        references=[resolve_ref(ref) for ref in spec['refs']],
        original_ledger_rows=rows,
    ))

assert all(item['exists'] for project in evidence for item in project['directories'])
counts = Counter(e['status'] for e in evidence)
assert counts == {'已完成': 13, '进行中': 5}
assert list(dict.fromkeys(t['p'] for t in board['tasks'])) == names
assert [t['s'] for t in board['tasks']] == [t['s'] for t in original['tasks']]
assert all(t['d'] and t['out'] and t['iss'] for t in board['tasks'])
assert len(board['keypoints']) == 18


text = TEMPLATE.read_text(encoding='utf-8-sig')


def replace(old, new, count=None):
    global text
    actual = text.count(old)
    if actual == 0 or (count is not None and actual != count):
        raise ValueError(f'Template replacement mismatch ({actual}): {old[:100]}')
    text = text.replace(old, new)


def sub(pattern, value, expected=1):
    global text
    text, n = re.subn(pattern, lambda _: value, text)
    assert n == expected, (pattern, n)


sub(r'const RAW = \[[\s\S]*?\n\];', 'const RAW = ' + json.dumps(board['tasks'], ensure_ascii=False, indent=2).replace('<', '\\u003c') + ';')
sub(r'const KEYPOINTS = \{[\s\S]*?\n\};', 'const KEYPOINTS = ' + json.dumps(board['keypoints'], ensure_ascii=False, indent=2) + ';')
replace("const CUTOFF_DEFAULT = '2026-10-13';", f"const CUTOFF_DEFAULT = '{AS_OF}';")
replace("const LS_KEY = 'audit-board-v1';", "const LS_KEY = 'audit-board-reconciled-20261021';")
replace('项目管理.xlsx', '原台账 + 日报')
replace('专项项目全周期进度与留痕管理平台', '专项进展、成果与待办 · 2026年10月21日梳理')
replace("const MONTHS = ['1月','2月','3月','4月','5月','6月','7月'];", "const MONTHS = ['1月','2月','3月','4月','5月','6月','7月','8月','9月','10月'];")
sub(r'const PHASE_ORDER = \[[^\n]*\];', 'const PHASE_ORDER = ' + json.dumps(list(dict.fromkeys(t['ph'] for t in board['tasks'])), ensure_ascii=False) + ';')
replace("if(!ds) return '—';", "if(!ds) return '待确认';", 1)
replace('const monthIdx = ds => parse(ds).getMonth();', 'const monthIdx = ds => parse(ds)?.getMonth() ?? -1;')
replace('if(!a || !b) return 0;', 'if(!a || !b) return null;')
replace('const starts = p.tasks.map(t=>t.st), ends = p.tasks.map(t=>t.en);', 'const starts = p.tasks.map(t=>t.st), ends = p.tasks.map(t=>t.en);')
replace('p.start = starts.sort()[0];', "p.start = starts.every(Boolean) ? starts.sort()[0] : ''; p.datesKnown = starts.every(Boolean) && ends.every(Boolean);")
replace('p.end = ends.sort().slice(-1)[0];', "p.end = ends.every(Boolean) ? ends.sort().slice(-1)[0] : '';")
replace("p.overdue = (p.status==='进行中') && (parse(p.end) < TODAY);", "p.overdue = p.datesKnown && (p.status==='进行中') && (parse(p.end) < TODAY);")
replace("if(state.range==='q3') return m>=6 && m<=8;", "if(state.range==='q3') return m>=6 && m<=8;\n  if(state.range==='q4') return m>=9 && m<=11;")
replace('<button data-r="q3">Q3</button>', '<button data-r="q3">Q3</button>\n      <button data-r="q4">Q4</button>')
replace("r.d+' '+r.c+' '+r.o", "r.d+' '+r.c+' '+r.o+' '+r.out+' '+r.iss")
replace('净投入', '区间历时')
replace('项目完成率', '台账完成率')
replace('<div class="t">完成率</div>', '<div class="t">阶段完成比例</div>')
replace('<span class="prog-txt">${p.done}/${p.total} · ${p.pct}%</span>', '<span class="prog-txt">阶段完成 ${p.done}/${p.total}</span>')
replace('<div class="foot">单项目最长 <b>${maxSpan}</b> 个工作日</div>', '<div class="foot">仅含已知日期 · 多项目可重叠</div>')
replace("${overdue?'进行中且已超预计完成日':'无逾期项目'}", "${overdue?'已知计划日期的逾期项目':'日期待确认项目不判定逾期'}")
replace('所有进行中项目均在计划工期内。', '仅对已知日期检查逾期；日期待确认的项目需另补计划。')
replace('const dur = [...projs].sort((a,b)=>b.spanDays-a.spanDays);', 'const dur = projs.filter(p=>p.datesKnown).sort((a,b)=>b.spanDays-a.spanDays);')
replace('按任务开始时间归集', '仅按已知开始时间归集；待确认日期不计入')
replace('工作日口径：各项目最早 ~ 最晚日期区间内，剔除周末与法定节假日', '仅统计已知日期的区间工作日，可相互重叠，不代表实际工时')
replace('${sp}', "${sp ?? '待确认'}")
replace('${p.spanDays}', "${p.spanDays ?? '待确认'}")
replace('workdays(t.st,t.en) +', "(workdays(t.st,t.en) ?? '待确认') +")
replace("if(!o.st || !o.en){ toastMsg('请填写开始时间与预计完成时间', 'err'); return; }", '// Dates may remain empty until the actual plan is confirmed.')
replace('if(parse(o.en) < parse(o.st))', 'if(o.st && o.en && parse(o.en) < parse(o.st))')
replace("    pg: 1,\n    st: $('m-st').value,", "    pg: $('m-s').value === '已完成' ? 1 : null,\n    st: $('m-st').value,")
replace('开始时间 <b>*</b>', '开始时间（可待确认）')
replace('预计完成时间 <b>*</b>', '预计完成时间（可待确认）')
replace("const l = pos(p.start), w = Math.max(1.2, pos(p.end)-pos(p.start));", "const l = p.datesKnown ? pos(p.start) : 1, w = p.datesKnown ? Math.max(1.2, pos(p.end)-pos(p.start)) : 27;")
replace('background:${c};${over}', "background:${p.datesKnown?c:'#64748b'};${over}")
replace('<span class="bt">${p.pct}%</span>', '<span class="bt">${p.datesKnown?p.pct+\'% 阶段完成\':\'起止日期待确认\'}</span>')
replace("const today = pos('2026-10-13');", 'const today = pos(CUTOFF);')
replace('content:"今日 10/13";', 'content:attr(data-label);')
replace('<div class="g-today" style=', '<div class="g-today" data-label="截至 ${CUTOFF}" style=')
sub(r'\s*<div class="g-gap"[^\n]*</div>', '')
replace('<span><span class="sw" style="background:#f1f4f8"></span>无记录区间</span>', '<span>灰色提示条仅标示日期待确认，不表示项目时间范围</span>')
replace('红色虚线为数据截至日 2026-10-13', '红色虚线为数据截至日 ${CUTOFF}')

notes = '''  $('notesBody').innerHTML = `
    <b>数据与状态</b>
    <ul><li>按用户当前台账的18个项目名称整理；当前共 ${TASKS.length} 条任务。完成状态沿用原台账，较早材料中的未闭环记录放在重点问题中。</li>
    <li>已检索2026年4月12日至10月20日的已保存日报。10月20日日报没有可用事项总结，最近有业务内容的日报为10月19日。历史补录按文件修改日归集，不能据此认定实际开工或完成日期。</li>
    <li>新增项目原填10月21日起止日期缺少依据，已留空；BOM一期9月3日上线单独作为明确里程碑记录。</li>
    <li>进度条表示已标完成阶段数÷阶段总数；进行中阶段的实际百分比未估算。日期未齐的项目不参加时长、逾期和月度启动统计。</li></ul>
    <b>成果与待办</b>
    <ul><li>“原台账载明”的成果按原有描述整理，本次未逐份复核定稿。日报、方案和会议纪要按各自语义保留；方案、预计收益与上线验收分别记录。</li>
    <li>区间历时沿用原看板工作日算法，表示日历区间内的工作日，可跨项目重叠；不代表实际投入工时。</li>
    <li>人员字段沿用原值。来源定位和改动明细见同目录梳理说明与梳理依据文件。</li></ul>`;'''
sub(r"  \$\('notesBody'\)\.innerHTML = `[\s\S]*?`;", notes)

opening = '''<div style="max-width:1440px;margin:14px auto 0;padding:14px 22px;background:#fff8eb;border:1px solid #e9d6ab;border-radius:8px;color:#654716;line-height:1.8;font-size:13px">
<b>18 个项目已按现有名称整理：原台账 13 个已完成、5 个进行中。</b><br>
补齐了 19 条空白工作内容和 42 条成果记录。9 个新增项目的起止日期待确认；完成状态沿用原台账，后续核对事项见各项目详情。
<a href="梳理说明.txt" style="margin-left:12px;color:#1f4e78">查看梳理说明</a>
</div>'''
replace('<nav class="viewnav" id="viewnav">', opening + '\n<nav class="viewnav" id="viewnav">')

# Reference links are local file links in the delivered offline HTML.
details = []
for item in evidence:
    links = ''.join(f'<li><a href="{html.escape(Path(r["path"]).as_uri(), quote=True)}">{html.escape(Path(r["path"]).name)}</a> · 第 {r["line"]} 行：{html.escape(r["locator"])}</li>' for r in item['references'])
    if not links:
        links = '<li>本次依据用户提供的原台账对应阶段记录；该项目原定时间早于现有日报起始日，或未作独立结项复核。</li>'
    details.append(f'<details style="margin:12px 0"><summary>{html.escape(item["project"])}</summary><p>{html.escape(item["summary"])}</p><p>后续：{html.escape(item["next"])}</p><ul>{links}</ul></details>')
source_panel = '<details class="notes" style="margin-top:16px"><summary>项目归类、依据与后续事项</summary><div class="nb">' + ''.join(details) + '</div></details>'
replace('      <div class="nb" id="notesBody"></div>\n    </details>', '      <div class="nb" id="notesBody"></div>\n    </details>\n' + source_panel)

# Data maintenance can change statuses directly without filling a percentage.
replace("TASKS[+sel.closest('tr').dataset.i].s = sel.value;", "TASKS[+sel.closest('tr').dataset.i].s = sel.value;\n  TASKS[+sel.closest('tr').dataset.i].pg = sel.value === '已完成' ? 1 : null;")

changes = []
for n, (before, after) in enumerate(zip(original['tasks'], board['tasks'], strict=True), 1):
    changed = {key: {'before': before.get(key), 'after': value} for key, value in after.items() if before.get(key) != value}
    if changed:
        changes.append({'row': n, 'project': after['p'], 'changes': changed})
manifest = dict(
    as_of=AS_OF,
    input={'path': str(SOURCE), 'sha256': source_hash},
    original_board={'path': str(TEMPLATE), 'sha256': template_hash},
    scope={'saved_daily_count': len(list((REPO/'data/reports').glob('2026-*/*.md'))), 'first_daily': '2026-04-12', 'last_daily': '2026-10-20', 'last_daily_with_business_content': '2026-10-19', 'empty_latest_reason': '10月20日材料基线晚于目标日，无可用事项总结。'},
    counts={'projects': len(names), 'tasks': len(board['tasks']), 'projects_by_original_status': dict(counts), 'tasks_by_original_status': dict(Counter(t['s'] for t in board['tasks'])), 'descriptions_filled': 19, 'outputs_filled': 42, 'dates_pending_rows': 19, 'unknown_progress_rows': 7},
    policies=['项目名称和任务状态沿用用户原台账。', '历史补录日不作实际项目起止日期。', '较早未闭环材料作为待核对项，不自动覆盖用户当前状态。', '缺失进度使用null，不估算完成百分比。', '原台账及原HTML不覆盖。'],
    projects=evidence, task_changes=changes,
)

lines = [
    '审计项目梳理｜2026年10月21日', '',
    '18个项目、42条阶段记录。按原台账状态：13个已完成、5个进行中。',
    '已补齐19条空白工作内容、42条成果记录，以及9个新增项目的关注要点。',
    '原文件保持原样。推荐双击同目录“审计项目看板-梳理版.html”查看，数据已内置。',
    '梳理版JSON用于本次配套看板的数据维护导入。原HTML对空日期有处理缺陷，请使用本次配套HTML。', '',
    '需要优先推进的5个项目',
]
for e in evidence:
    if e['status'] == '进行中':
        lines.extend([e['project'], '  当前：' + e['summary'], '  后续：' + e['next'], ''])
lines.extend([
    '已完成项目中的后续核对',
    '• OA三级审批：原台账标为已完成；10月10日及15日材料仍有配置、职责讨论记录，需补最新验收或范围说明。',
    '• 销售BOM一期：9月3日上线有明确记载；10月10日审批意见反写缺新单据验证。',
    '• 呆滞料：10月10日材料中的处理建议选项、说明配置仍有待确认项。',
    '• ERP权限及倍可尽调：原台账标为已完成；本次找到分析/问题汇总成果，最终批准、上线或正式结项文件需补定位。', '',
    '本次数据处理',
    '1. 18个项目名称和42条任务状态全部保留，推进人继续沿用原值。',
    '2. 募集资金专项从“尽调专项”调整为“审计专项”。',
    '3. 倍可第三条重复的“访谈与资料检查”调整为“总结汇报”。',
    '4. 募集资金、供应商、EAM和销售BOM二期按当前工作内容调整阶段名称。',
    '5. 9个新增项目的19条记录原填起止日均为10月21日，缺少项目依据，改为待确认；原有23条记录的日期保留。',
    '6. 7条进行中记录的进度从1改为null（未知）。看板进度条按已标完成阶段比例计算，并注明口径。',
    '7. 看板副本支持空日期；空日期不计逾期、月度和历时。更新旧的固定项目数、截止日和“8—10月无新增项目”说明。',
    '8. 看板中的区间工作日可跨项目重叠，名称改为“区间历时”，不作为实际工时。', '',
    '依据与日期口径',
    '已检索4月12日至10月20日共192份已保存日报。10月20日没有可用事项总结，最近有业务内容的日报为10月19日。',
    '历史日报多为按文件修改日补录；记录出现日期、源文档业务日期、实际项目起止日分别处理。',
    '保留原台账的当前状态；较早材料中未闭环事项标明其日期，避免将历史问题直接当作当前事实。',
    '“原台账载明”的成果来自用户原有描述，本次未逐份验证正式定稿。方案和预计收益不写作已实现成效。',
    '销售BOM一期不同版本存在节点/层级及统计样本差异，梳理中保留9月3日上线和节点字段精简，不混用节省工时数字。', '',
    '逐项目对应关系和来源', '',
])
for i, e in enumerate(evidence, 1):
    lines.extend([f'{i:02d}. {e["project"]}', f'状态：{e["status"]}（原台账）；依据：{e["basis"]}', '工作：'+e['summary'], '后续：'+e['next'], '目录：'])
    lines.extend('  '+folder['path'] for folder in e['directories'])
    lines.append('原台账任务行：'+', '.join(map(str,e['task_rows'])))
    for ref in e['references']:
        lines.extend([f'来源：{ref["path"]}（第{ref["line"]}行）', '定位：'+ref['locator']])
    lines.append('')

DEST.mkdir(parents=True, exist_ok=True)
outputs = {
    '审计项目看板-数据-梳理版.json': json.dumps(board, ensure_ascii=False, indent=2)+'\n',
    '审计项目看板-梳理版.html': text,
    '梳理说明.txt': '\n'.join(lines)+'\n',
    '梳理依据.json': json.dumps(manifest, ensure_ascii=False, indent=2)+'\n',
}
for filename, content in outputs.items():
    (DEST/filename).write_text(content, encoding='utf-8', newline='\n')

# Validate the delivered data and its embedding, not the unrelated Rust runtime.
readback = json.loads((DEST/'审计项目看板-数据-梳理版.json').read_text(encoding='utf-8'))
embedded = re.search(r'const RAW = (\[[\s\S]*?\n\]);', text)
assert embedded and json.loads(embedded.group(1)) == readback['tasks']
assert sha(SOURCE) == source_hash and sha(TEMPLATE) == template_hash
assert set(readback) == set(original)
assert all(set(a) == set(b) for a,b in zip(readback['tasks'],original['tasks'],strict=True))
print(json.dumps({'output':str(DEST),'files':[{'name':name,'bytes':(DEST/name).stat().st_size} for name in outputs],'counts':manifest['counts'],'input_preserved':True,'html_data_matches_json':True},ensure_ascii=False,indent=2))
