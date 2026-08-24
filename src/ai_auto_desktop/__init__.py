"""Public API for ai-auto-desktop."""

from .compiler import API_VERSION, KIND, compile_descriptor, load_descriptor
from .durable import DurableExecutionResult, DurableExecutor
from .errors import AutomationError, DescriptorError, DescriptorIssue
from .model import CompiledStep, RunResult, WorkflowDescriptor
from .journal import (
    DesiredState,
    EventRecord,
    JournalStore,
    OwnerLease,
    RunRecord,
    RunStatus,
)
from .plugin import PluginError, ProcessPlugin
from .run_service import DispatchState, RunService, RunServiceError
from .runtime import WorkflowRunner, run_descriptor

__version__ = "0.1.0"

__all__ = [
    "API_VERSION", "KIND", "AutomationError", "CompiledStep", "DescriptorError",
    "DescriptorIssue", "DesiredState", "DispatchState",
    "DurableExecutionResult", "DurableExecutor", "EventRecord",
    "JournalStore", "OwnerLease", "PluginError", "ProcessPlugin",
    "RunRecord", "RunResult", "RunService", "RunServiceError",
    "RunStatus", "WorkflowDescriptor", "WorkflowRunner",
    "compile_descriptor", "load_descriptor", "run_descriptor",
]
