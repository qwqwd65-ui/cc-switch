# 原子页面切换设计

日期：2026-09-25

## 目标

页面导航时让 Header 和内容区域在同一次 React 提交中完成切换，不保留旧页面、不播放整页流程动画，也不暴露透明或空白的中间帧。

## 根因

- `App.tsx` 使用 `AnimatePresence mode="wait"` 包裹 `currentView` 内容，旧页面先淡出 200ms，新页面再淡入 200ms；Header 不在这个动画内，因此会先显示新状态，而内容仍是旧页面。
- `SettingsPage` 始终以 `general` 初始化活动标签，再通过普通 `useEffect` 切到 `defaultTab`，导致打开“使用统计”等标签时先绘制一次“通用”。
- 供应商查询使用 `keepPreviousData`，切换应用后会暂时把上一应用的模型列表交给新页面渲染。

## 设计

1. 移除 `currentView` 外层的 `AnimatePresence` 和 `motion.div`，改为带稳定背景色的普通内容容器。
2. 移除供应商列表及其 Header 操作区的 `mode="wait"`，并取消供应商查询的 `keepPreviousData`；应用切换后直接显示目标应用的现有骨架屏。
3. 移除设置标签内容、Skills 发现页和主内容容器的流程淡入动画，不添加替代过渡；弹窗和按钮反馈等局部交互动画保持不变。
4. `SettingsPage` 使用 `defaultTab` 初始化活动标签，并用 `useLayoutEffect` 在后续目标标签变化时于绘制前同步。
5. 保留各页面自己的加载状态，切换后直接显示目标页面或其加载占位，不显示来源页面。

## 验证

- App 集成测试断言进入设置与返回主页后，来源页面内容立即从 DOM 移除。
- App 集成测试断言切换应用后，旧应用供应商数据立即消失。
- SettingsPage 组件测试记录首次 Tabs 渲染值，断言首帧就是传入的 `defaultTab`。
- 本机只执行格式化和文本级检查；完整测试、类型检查、构建与安装包生成由 GitHub Actions 完成。
