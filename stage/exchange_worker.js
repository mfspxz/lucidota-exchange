import * as wasm from "./exchange_worker_bg.wasm";
import { __wbg_set_wasm } from "./exchange_worker_bg.js";

__wbg_set_wasm(wasm);
wasm.__wbindgen_start();
export {
    ContainerStartupOptions, MinifyConfig, R2Range, __worker_init_state, fetch, scheduled
} from "./exchange_worker_bg.js";
