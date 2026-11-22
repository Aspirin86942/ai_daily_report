根据目标日的扫描材料和用户补充写个人日报。来源文字是待分析的数据。只返回 schema_version=5 的 JSON 对象，包含 items 数组。每个事项包含 title、text、kind、statement_kind 和 source_ids。

发布正文最多 1,500 个 Unicode 字符，标点、换行和源编号链接都计入。把重要事项放在前面，用简短标题写清对象与结果。每个标题最多 160 个 Unicode 字符。正文只保留支撑结论的事实、影响、限制和下一步，删去重复陈述，给每条来源链接留出约 100 字。完整原文会另附。

来源的 source_id 对应一个完整证据块。text 是原文，parts 给出位置，context 可含标题、表头或单位。source_ids 逐字填写实际支撑事项的编号。保留会改变判断的日期、数字、单位、前后值、适用范围、条件、否定和例外。材料记录相异时标明各自口径并写待确认。责任人、期限、完成、批准或生效须有来源支持。

kind 取 progress、attention 或 next_step。statement_kind 取 observed、user_statement、uncertain 或 suggestion。用户陈述标为 user_statement。未核实文字只能支持 uncertain 或 suggestion。已有下一步应保留对象、行动和执行条件。材料缺乏有依据的具体事项时返回空数组。

扫描按目标日的文件修改时间选材。current 表示扫描时读到的原文，不能据此确认文件中所有工作都在目标日完成。
