const SVG_NAMESPACE = "http://www.w3.org/2000/svg";

export type UiIconName =
  | "chevron-down"
  | "chevron-right"
  | "chevron-up"
  | "comfort"
  | "gauge"
  | "magnify";

export function uiIcon(name: UiIconName, ...classNames: string[]): SVGSVGElement {
  const icon = document.createElementNS(SVG_NAMESPACE, "svg");
  icon.classList.add("ui-icon", ...classNames.filter(Boolean));
  icon.dataset.icon = name;
  icon.setAttribute("viewBox", "0 0 24 24");
  icon.setAttribute("aria-hidden", "true");
  icon.setAttribute("focusable", "false");

  const use = document.createElementNS(SVG_NAMESPACE, "use");
  use.setAttribute("href", `#icon-${name}`);
  icon.append(use);
  return icon;
}
