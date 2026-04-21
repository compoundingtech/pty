// Public surface for the widgets tier — higher-level components built on
// the core TUI primitives. All state-first: you own the state, widgets are
// pure render + pure key dispatch.

export {
  type TreeNode, type TreeRow, type TreeState,
  createTreeState, flattenTree, toggleExpanded, selectById,
  moveSelection, handleTreeKey, treeGlyph,
} from "./tree.ts";

export {
  type DatePickerState,
  MONTH_NAMES, daysInMonth,
  datePickerFromDate, clampDay,
  shiftDay, shiftMonth, shiftTime,
  toDate, handleDatePickerKey,
  calendarCanvas, datePickerBody, datePickerPanel,
} from "./date-picker.ts";

export {
  type TextFieldState, type FormState, type HandleFormKeyResult,
  applyTextKey, renderFieldText, renderFieldNodes,
  prevWordBoundary, nextWordBoundary,
  createFormState, focusField, setFieldText, handleFormKey,
} from "./form.ts";

export {
  type MarkdownOptions,
  parseMarkdown, parseInline, renderMarkdown,
} from "./markdown.ts";

export {
  type TextAreaState,
  createTextArea, textAreaToString, applyTextAreaKey, renderTextArea,
} from "./text-area.ts";

export {
  type VirtualListState, type VirtualWindow,
  type HandleVirtualKeyResult, type HandleVirtualMouseResult,
  createVirtualListState, clampVirtual, virtualWindow,
  moveVirtualSelection, pageVirtual,
  jumpVirtualToStart, jumpVirtualToEnd, handleVirtualKey, handleVirtualMouse,
  renderVirtualList,
} from "./virtual-list.ts";

export {
  type StreamViewState,
  createStreamView, isPinned as streamIsPinned, streamPin,
  streamScrollUp, streamScrollDown, streamWindow,
  handleStreamKey, handleStreamMouse, renderStreamView,
} from "./stream-view.ts";

export {
  type TabDef, type TabsState,
  createTabsState, selectTab, nextTab, prevTab,
  handleTabsKey, handleTabsMouse,
  renderTabs,
} from "./tabs.ts";

export {
  type ConfirmState, type CreateConfirmOptions, type HandleConfirmResult,
  createConfirm, handleConfirmKey, confirmPanel,
} from "./confirm.ts";

export {
  type Toast, type ToastKind, type ToastQueue, type PushToastOptions,
  createToastQueue, pushToast, pruneExpired, dismissToast, renderToasts,
} from "./toast.ts";

export {
  type Command, type CommandPaletteState, type RankedCommand,
  type HandleCommandPaletteResult,
  createCommandPaletteState, filterCommands,
  handleCommandPaletteKey, renderCommandPalette,
} from "./command-palette.ts";

export {
  registerGlobalCommand, useCommandScope, clearCommandScope,
  findCommand, runCommand, allCommands,
  _resetCommandRegistry,
} from "./command-registry.ts";

export {
  type TableColumn, type TableState, type TableAlign,
  type HandleTableKeyResult,
  createTableState, sortRows, handleTableKey, renderTable,
} from "./table.ts";

export {
  type HelpSection, type HelpBinding,
  helpPanel,
} from "./help-overlay.ts";

export {
  type PromptBarValue, type PromptBarTitle, type PromptBarStatus,
  type PromptBarOptions, type TitleAlign,
  promptBar,
} from "./prompt-bar.ts";

export {
  type ToolbarItem, type ToolbarOptions,
  toolbar, toolbarItemFor,
} from "./toolbar.ts";

export {
  type SparklineOptions,
  sparkline, sparklineString,
} from "./sparkline.ts";

export {
  type BarChartItem, type BarChartOptions,
  barChart,
} from "./bar-chart.ts";
