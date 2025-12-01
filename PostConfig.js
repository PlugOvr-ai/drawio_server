/**
 * Copyright (c) 2006-2024, JGraph Holdings Ltd
 * Copyright (c) 2006-2024, draw.io AG
 */
// null'ing of global vars need to be after init.js
window.VSS_CONVERT_URL = null;
window.EMF_CONVERT_URL = null;
window.ICONSEARCH_PATH = null;

// Log the absolute position of the geBackgroundPage div every second
(function() {
    function logBackgroundPagePosition() {
        var backgroundPage = document.querySelector('.geBackgroundPage');
        if (backgroundPage) {
            var rect = backgroundPage.getBoundingClientRect();
            var scrollX = window.pageXOffset || document.documentElement.scrollLeft || document.body.scrollLeft || 0;
            var scrollY = window.pageYOffset || document.documentElement.scrollTop || document.body.scrollTop || 0;
            
            var absoluteX = rect.left + scrollX;
            var absoluteY = rect.top + scrollY;
            
            console.log('geBackgroundPage absolute position:', {
                x: absoluteX,
                y: absoluteY,
                left: absoluteX,
                top: absoluteY,
                width: rect.width,
                height: rect.height,
                right: absoluteX + rect.width,
                bottom: absoluteY + rect.height
            });
        } else {
            console.log('geBackgroundPage element not found');
        }
    }
    
    // Start logging every second
    setInterval(logBackgroundPagePosition, 1000);
})();
