// Each invocation gets a new JS realm. All browser calls pass through the
// session bound by the operator. Return values are plain JSON, including rows.
(source, parameters, dispatch) => {
    "use strict";
    const parse = JSON.parse;
    const stringify = JSON.stringify;
    const call = (method, args) => {
        const reply = parse(dispatch(stringify({method, ...args})));
        if (reply.error) throw new Error(reply.error.message);
        return reply.data;
    };
    const act = (locator, action) => call("act", {locator, action});
    const fields = fields => call("fields", {fields}).values;
    const read = (locator, property = "value") => fields([{as: "value", locator, property}]).value;
    const page = Object.freeze({
        observe: (mode = "outline") => call("observe", {mode}),
        read, fields, act,
        present: locator => read(locator, "present"),
        click: locator => act(locator, {type: "click"}),
        fill: (locator, value) => act(locator, {type: "fill", value}),
        upload: (locator, files) => act(locator, {type: "upload", files}),
        key: (locator, keys) => act(locator, {type: "key", keys}),
        scroll: (locator, direction, notches = 1) => act(locator, {type: "scroll", direction, notches}),
        wait: (locator, condition = "appears", states) => call("wait", {locator, condition, states}),
        table: (locator, columns) => call("table", {locator, columns}),
        collect: spec => call("collect", {spec})
    });
    const execute = Function("page", "inputs", '"use strict";\n' + source);
    const value = execute(page, parse(parameters));
    if (value && typeof value.then === "function") throw new TypeError("page methods are synchronous; return a JSON value");
    const result = stringify(value === undefined ? null : value);
    if (typeof result !== "string") throw new TypeError("return a JSON value");
    return result;
}
