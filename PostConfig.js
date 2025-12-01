/**
 * Copyright (c) 2006-2024, JGraph Holdings Ltd
 * Copyright (c) 2006-2024, draw.io AG
 */
// null'ing of global vars need to be after init.js
window.VSS_CONVERT_URL = null;
window.EMF_CONVERT_URL = null;
window.ICONSEARCH_PATH = null;

// Send geBackgroundPage position and mouse coordinates relative to geBackgroundPage origin
(function() {
    // Only run if we're in an iframe
    if (window.self === window.top) {
        return;
    }
    
    var lastSentTime = 0;
    var throttleMs = 33; // ~30fps to avoid flooding
    
    function sendBackgroundPagePosition() {
        var backgroundPage = document.querySelector('.geBackgroundPage');
        if (backgroundPage) {
            var rect = backgroundPage.getBoundingClientRect();
            var scrollX = window.pageXOffset || document.documentElement.scrollLeft || document.body.scrollLeft || 0;
            var scrollY = window.pageYOffset || document.documentElement.scrollTop || document.body.scrollTop || 0;
            
            var absoluteX = rect.left + scrollX;
            var absoluteY = rect.top + scrollY;
            
            var positionData = {
                event: 'backgroundPagePosition',
                x: absoluteX,
                y: absoluteY,
                left: absoluteX,
                top: absoluteY,
                width: rect.width,
                height: rect.height,
                right: absoluteX + rect.width,
                bottom: absoluteY + rect.height,
                clientLeft: rect.left,
                clientTop: rect.top
            };
            
            // Send to parent window
            var parent = window.opener || window.parent;
            if (parent && parent.postMessage) {
                parent.postMessage(JSON.stringify(positionData), '*');
            }
            
            console.log('geBackgroundPage absolute position:', positionData);
        } else {
            console.log('geBackgroundPage element not found');
        }
    }
    
    function sendMousePosition(e) {
        var now = performance.now();
        if (now - lastSentTime < throttleMs) {
            return;
        }
        lastSentTime = now;
        
        try {
            var backgroundPage = document.querySelector('.geBackgroundPage');
            if (!backgroundPage) {
                return;
            }
            
            var bgPageRect = backgroundPage.getBoundingClientRect();
            
            // Calculate absolute coordinates relative to geBackgroundPage origin
            // This is the mouse position relative to the top-left corner of geBackgroundPage
            // e.clientX/Y are relative to viewport, bgPageRect.left/top are also relative to viewport
            var absoluteX = e.clientX - bgPageRect.left;
            var absoluteY = e.clientY - bgPageRect.top;
            
            // Debug logging (occasionally to avoid spam)
            if (Math.random() < 0.01) {
                console.log('PostConfig sendMousePosition debug:', {
                    clientX: e.clientX,
                    clientY: e.clientY,
                    bgPageRect: { left: bgPageRect.left, top: bgPageRect.top },
                    absolute: { x: absoluteX, y: absoluteY }
                });
            }
            
            var message = {
                event: 'mouseMove',
                absoluteX: absoluteX,  // Relative to geBackgroundPage origin
                absoluteY: absoluteY, // Relative to geBackgroundPage origin
                clientX: e.clientX,
                clientY: e.clientY
            };
            
            // Send to parent window
            var parent = window.opener || window.parent;
            if (parent && parent.postMessage) {
                parent.postMessage(JSON.stringify(message), '*');
            }
        } catch (err) {
            console.debug('Mouse tracking error:', err);
        }
    }
    
    // Send position every second
    setInterval(sendBackgroundPagePosition, 1000);
    
    // Listen for mouse move events
    document.addEventListener('mousemove', sendMousePosition, { passive: true });
    
    // Also send immediately and after delays to catch initial render
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', function() {
            setTimeout(sendBackgroundPagePosition, 100);
            setTimeout(sendBackgroundPagePosition, 500);
        });
    } else {
        setTimeout(sendBackgroundPagePosition, 100);
        setTimeout(sendBackgroundPagePosition, 500);
    }
})();
