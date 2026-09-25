# 原子页面切换设计

日期：2026-09-25

## 目标

页面导航时让 Header 和内容区域在同一次 React 提交中完成切换，不保留旧页面、不播放整页流程动画，也不暴露透明或空白的中间帧。

## 根因

- `App.tsx` 使用 `AnimatePresence mode="wait"` 包裹 `currentView` 内容，旧页面先淡出 200ms，新页面再淡入 200ms；Header 不在这个动画内，因此会先显示新状态，而内容仍是旧页面。
- `SettingsPage` 始终以 `general` 初始化活动标签，再通过普通 `useEffect` 切到 `defaultTab`，导致打开“使用统计”等标签时先绘制一次“通用”。

## 设计

1. 移除 `currentView` 外层的 `AnimatePresence` 和 `motion.div`，改为带稳定背景色的普通内容容器。
2. 不为整页导航增加替代动画；页面内部现有的列表、弹窗和控件动画保持不变。
3. `SettingsPage` 使用 `defaultTab` 初始化活动标签，并用 `useLayoutEffect` 在后续目标标签变化时于绘制前同步。
4. 保留各页面自己的加载状态，切换后直接显示目标页面或其加载占位，不显示来源页面。

## 验证

- App 集成测试断言进入设置与返回主页后，来源页面内容立即从 DOM 移除。
- SettingsPage 组件测试记录首次 Tabs 渲染值，断言首帧就是传入的 `defaultTab`。
- 本机只执行格式化和文本级检查；完整测试、类型检查、构建与安装包生成由 GitHub Actions 完成。
