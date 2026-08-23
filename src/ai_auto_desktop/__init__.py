"""Public API for ai-auto-desktop."""

from .compiler import API_VERSION, KIND, compile_descriptor, load_descriptor
from .errors import AutomationError, DescriptorError, DescriptorIssue
from .model import CompiledStep, RunResult, WorkflowDescriptor
from .plugin import PluginError, ProcessPlugin
from .runtime import WorkflowRunner, run_descriptor

__version__ = "0.1.0"

__all__ = [
    "API_VERSION", "KIND", "AutomationError", "CompiledStep", "DescriptorError",
    "DescriptorIssue", "PluginError", "ProcessPlugin", "RunResult",
    "WorkflowDescriptor", "WorkflowRunner", "compile_descriptor",
    "load_descriptor", "run_descriptor",
]
