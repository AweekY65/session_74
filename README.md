# bitmap-index

本地压缩整数 Bitmap Index（Roaring 风格），用于对大量非负 32 位整数集合执行高效集合运算。
纯 Rust 标准库实现：所有数据只保存在内存或本地文件中，不依赖数据库、现成 RoaringBitmap 库或任何外部服务。

## Container 设计

- 每个 `u32` 拆分为 **高 16 位（桶号）** 和 **低 16 位（桶内值）**。
- 顶层用 `BTreeMap<u16, Container>` 按桶号组织，天然支持有序迭代。
- 每个桶（container）有两种表示，按密度自动选择：
  - **Array**：有序 `Vec<u16>`，二分查找。稀疏时每个值仅占 2 字节。
  - **Bitmap**：`[u64; 1024]`，共 65536 bit = 8192 字节。稠密时每个值摊销 < 1 bit 级开销。

## 转换策略

- 阈值 `ARRAY_LIMIT = 4096`：array 最多 4096 个值（8 KB），与 bitmap 的 8192 字节持平。
- `add` 使 array 超过 4096 → 转为 bitmap；`remove` 使 bitmap 基数降到 4096 及以下 → 转回 array。
- 集合运算（union/intersection/difference/xor）按结果基数选择最紧凑表示；bitmap×bitmap 走按位机器字运算（OR/AND/ANDNOT/XOR，每步 64 值）。
- 转换只改变物理表示，不改变集合内容（由 `container conversion` 差分场景验证）。

## 复杂度（单桶内，n/m 为两 container 基数）

| 操作 | Array | Bitmap |
|---|---|---|
| add / remove / contains | O(log n) 查找，插入 O(n) 移动 | O(1) |
| 集合运算 | O(n + m) 归并 | O(1024) 机器字运算 |
| 有序迭代 | O(n) | O(1024) 扫描，跳过空字 |

跨桶操作为对应桶操作的线性组合；`union`/`xor` 对桶做归并，`intersection` 只遍历较小的一侧。

## 序列化格式

```
magic    "BIDX" (4B)
version  u32 LE (=1)
buckets  u32 LE
per bucket: key u16 | kind u8 (0=array,1=bitmap) | card u32 | payload
payload:  array = card × u16（严格升序）；bitmap = 1024 × u64
crc32    u32 LE（IEEE，覆盖此前全部字节）
```

加载时依次校验 CRC-32、magic、版本、桶号升序、array 严格升序、bitmap popcount 与 card 一致、无尾部垃圾；任何损坏都会返回错误而非产生错误数据。

## API 概览

`add` / `remove` / `contains` / `cardinality` / `is_empty` / `iter`（升序）/
`union` / `intersection` / `difference` / `xor` /
`stored_bytes`（container 存储字节统计）/ `serialized_size` /
`to_bytes` / `from_bytes` / `save` / `load`。

## 测试

差分验证程序以 `BTreeSet` 为标准实现，覆盖稀疏、稠密、空集合、边界整数（0、65535、65536、`u32::MAX`）、随机增删、六种数据形态的四种集合运算、container 阈值转换、序列化往返、字节翻转/截断/尾部垃圾/错误版本等损坏输入，以及存储字节统计。

```bash
cargo run --release --bin bitmap_verify
```

终端逐场景输出 `PASS`/`FAIL`，任一失败以非零码退出。
