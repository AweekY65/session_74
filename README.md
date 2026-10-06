# bitmap_index

本地压缩整数 Bitmap Index：对大量非负 `u32` 整数集合提供高效的成员查询与集合运算。
纯 Rust 标准库实现，不依赖数据库、RoaringBitmap 现成库或任何外部服务；所有数据只保存在内存或本地文件中。

## Container 设计

整数按高 16 位分桶（`key = value >> 16`），每个桶是一个 container，保存低 16 位，
container 之间用 `BTreeMap<u16, Container>` 组织，天然支持按 key 有序合并。container 有两种表示：

- **Array**：有序 `Vec<u16>`，二分查找。稀疏时每个值仅占 2 字节。
- **Bitmap**：固定 65536 位（1024 个 `u64`，8 KiB）加缓存的基数。稠密集合的位运算极快。

## 转换策略

阈值为 **4096**（此时两种表示的负载都是 8 KiB，是天然的密度平衡点）：

- Array 插入后基数超过 4096 → 转为 Bitmap；
- Bitmap 删除后基数降到 4096 及以下 → 转回 Array；
- 集合运算（union/intersection/difference/xor）产生的新 container 按结果基数 `normalize`，
  自动选择更省的表示；与 Bitmap 参与的运算直接以位运算完成，避免逐元素展开。

转换只改变内部表示，不改变集合内容（由差分测试验证）。

## 复杂度

设单桶基数为 `k`，桶数为 `c`：

| 操作 | 复杂度 |
| --- | --- |
| `add` / `remove` / `contains` | Array：O(log k) 查找 + O(k) 插入移动；Bitmap：O(1) |
| `cardinality` | O(c)（Bitmap 基数已缓存） |
| 有序迭代 | O(结果基数)，严格升序 |
| 集合运算 | 按 key 归并；同 key 内 Array 归并 O(k1+k2)，Bitmap 按 1024 字位运算 O(1024) |
| 序列化 | O(存储字节数) |

`storage_bytes()` 返回内存表示的近似字节数（container 负载 + 每桶 key 开销），
稀疏数据约为 `2 × 基数`，稠密数据约为 `8 KiB × 稠密桶数`，不会退化为全量哈希表。

## 序列化格式

```
magic      : 4 字节 "RBMP"
version    : u32 = 1（不支持的版本拒绝加载）
containers : u32 桶数量
每桶       : key u16 | kind u8 (0=array, 1=bitmap) | cardinality u32 | 负载
crc32      : u32，对之前全部字节的 CRC-32 (IEEE)
```

加载时校验 magic、版本、结构长度、container 基数与负载一致性、CRC 以及尾随字节；
任何损坏（位翻转、截断、篡改头部、追加垃圾）都会返回明确的 `DeserializeError`。

## API 概览

- 单值：`add` / `remove` / `contains` / `cardinality` / `is_empty` / `iter`（升序）
- 集合：`union` / `intersection` / `difference` / `xor`
- 持久化：`to_bytes` / `from_bytes` / `save_to_file` / `load_from_file`
- 统计：`storage_bytes` / `container_count`

## 测试

独立差分验证程序，用确定性随机数据对照标准库 `BTreeSet`，覆盖稀疏、稠密、
空集合、边界整数（0、65535、65536、`u32::MAX`）、随机增删查、跨密度集合运算、
container 转换、序列化往返和损坏文件检测：

```sh
cargo run --bin bitmap_verify
# 或优化模式
cargo run --release --bin bitmap_verify
```

每组测试在终端输出 `PASS`/`FAIL`，全部通过时退出码为 0，任一失败退出码为 1。
